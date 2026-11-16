use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::{
    RsaPrivateKey,
    pkcs8::{EncodePrivateKey, EncodePublicKey},
    traits::PublicKeyParts,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::IntoFuture,
    sync::{Arc, Mutex},
};
use tokomotive::{config::Config, state::AppState};

struct Browser {
    client: reqwest::Client,
    cookies: HashMap<String, Cookie>,
}
struct Cookie {
    value: String,
    host: String,
    path: String,
    secure: bool,
}
impl Browser {
    fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            cookies: HashMap::new(),
        }
    }
    async fn send(
        &mut self,
        method: reqwest::Method,
        url: &str,
        form: &[(&str, &str)],
    ) -> reqwest::Response {
        let target = reqwest::Url::parse(url).unwrap();
        let cookie = self
            .cookies
            .iter()
            .filter(|(_, v)| {
                target.host_str() == Some(v.host.as_str())
                    && (!v.secure || target.scheme() == "https")
                    && (target.path() == v.path
                        || target
                            .path()
                            .starts_with(&format!("{}/", v.path.trim_end_matches('/'))))
            })
            .map(|(k, v)| format!("{k}={}", v.value))
            .collect::<Vec<_>>()
            .join("; ");
        let mut request = self
            .client
            .request(method.clone(), url)
            .header("cookie", cookie);
        if method == reqwest::Method::POST {
            request = request.form(form);
        }
        let r = request.send().await.unwrap();
        for h in r.headers().get_all("set-cookie") {
            let (k, v) = h
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap();
            let attributes = h
                .to_str()
                .unwrap()
                .split(';')
                .map(str::trim)
                .collect::<Vec<_>>();
            if attributes.contains(&"Max-Age=0") {
                self.cookies.remove(k);
            } else {
                self.cookies.insert(
                    k.into(),
                    Cookie {
                        value: v.into(),
                        host: target.host_str().unwrap().into(),
                        path: attributes
                            .iter()
                            .find_map(|a| a.strip_prefix("Path="))
                            .unwrap_or("/")
                            .into(),
                        secure: attributes.contains(&"Secure"),
                    },
                );
            }
        }
        r
    }
    async fn get(&mut self, url: &str) -> reqwest::Response {
        self.send(reqwest::Method::GET, url, &[]).await
    }
    async fn post(&mut self, url: &str, f: &[(&str, &str)]) -> reqwest::Response {
        self.send(reqwest::Method::POST, url, f).await
    }
}
struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for App {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let db = tokomotive::db::connect(&cfg.server.database).await.unwrap();
    tokomotive::db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new(&cfg, db, None).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let server = axum::serve(
        l,
        tokomotive::router(st.clone())
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .into_future();
    let task = tokio::spawn(async move {
        server.await.unwrap();
    });
    App {
        _dir: dir,
        st,
        url,
        task,
    }
}
struct Key {
    pem: String,
    public_pem: String,
    jwk: Value,
}
fn key(kid: &str) -> Key {
    let k = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    Key {
        pem: k.to_pkcs8_pem(Default::default()).unwrap().to_string(),
        public_pem: k
            .to_public_key()
            .to_public_key_pem(Default::default())
            .unwrap(),
        jwk: json!({"kty":"RSA","kid":kid,"alg":"RS256","use":"sig","n":B64.encode(k.n().to_bytes_be()),"e":B64.encode(k.e().to_bytes_be())}),
    }
}
struct Provider {
    base: String,
    key: Key,
    mode: String,
    sub: String,
    groups: Vec<String>,
    nonce: String,
    challenge: String,
    jwks_calls: usize,
    pkce_calls: usize,
    userinfo_calls: usize,
    post_auth: bool,
}
struct Mock {
    state: Arc<Mutex<Provider>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn mock() -> Mock {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = Arc::new(Mutex::new(Provider {
        base: format!("http://{}", l.local_addr().unwrap()),
        key: key("first"),
        mode: String::new(),
        sub: "subject-one".into(),
        groups: vec!["operators".into()],
        nonce: String::new(),
        challenge: String::new(),
        jwks_calls: 0,
        pkce_calls: 0,
        userinfo_calls: 0,
        post_auth: false,
    }));
    let router = Router::new()
        .route("/authorize",get(|State(p):State<Arc<Mutex<Provider>>>,axum::extract::Query(q):axum::extract::Query<HashMap<String,String>>|async move {
            let mut p=p.lock().unwrap();assert_eq!(q["response_type"],"code");assert_eq!(q["client_id"],"dashboard");assert_eq!(q["code_challenge_method"],"S256");
            p.nonce=q["nonce"].clone();p.challenge=q["code_challenge"].clone();
            let mut callback=reqwest::Url::parse(&q["redirect_uri"]).unwrap();callback.query_pairs_mut().extend_pairs([("state",q["state"].as_str()),("code","valid-code")]);
            axum::response::Redirect::to(callback.as_str())
        }))
        .route("/.well-known/openid-configuration",get(|State(p):State<Arc<Mutex<Provider>>>| async move {
            use axum::response::IntoResponse;
            let p=p.lock().unwrap();
            if p.mode=="discovery-redirect" { return axum::response::Redirect::temporary(&format!("{}/userinfo",p.base.replace("127.0.0.1","localhost"))).into_response(); }
            if p.mode=="discovery-large" { return Json(json!({"padding":"x".repeat(1024*1024+1)})).into_response(); }
            Json(json!({"issuer":if p.mode=="discovery-issuer" {"https://other.example"} else {&p.base},"authorization_endpoint":format!("{}/authorize",p.base),"token_endpoint":if p.mode=="discovery-public-http" {"http://192.0.2.1/token".into()} else {format!("{}/token",p.base)},"jwks_uri":format!("{}/jwks",p.base),"userinfo_endpoint":format!("{}/userinfo",p.base)})).into_response()
        }))
        .route("/jwks",get(|State(p):State<Arc<Mutex<Provider>>>| async move { let mut p=p.lock().unwrap();p.jwks_calls+=1;Json(json!({"keys":[p.key.jwk]})) }))
        .route("/token",post(|State(p):State<Arc<Mutex<Provider>>>,headers:axum::http::HeaderMap, axum::Form(f):axum::Form<HashMap<String,String>>| async move {
            let mut p=p.lock().unwrap();
            if p.post_auth {assert_eq!(f.get("client_id").map(String::as_str),Some("dashboard"));assert_eq!(f.get("client_secret").map(String::as_str),Some("private-client-secret"));assert!(!headers.contains_key("authorization"));} else {assert_eq!(headers["authorization"],format!("Basic {}",base64::engine::general_purpose::STANDARD.encode("dashboard:private-client-secret")));assert!(!f.contains_key("client_secret"));}
            if f.get("code_verifier").is_none_or(|v| B64.encode(Sha256::digest(v.as_bytes())) != p.challenge) { return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"}))); }
            p.pkce_calls+=1;
            let now=tokomotive::db::now_s();
            let mut claims=json!({"iss":p.base,"sub":p.sub,"aud":"dashboard","exp":now+300,"iat":now,"nbf":now-5,"nonce":p.nonce,"groups":p.groups,"email":"same@example.com"});
            match p.mode.as_str() { "nonce"=>claims["nonce"]=json!("wrong"),"aud"=>claims["aud"]=json!("other"),"iss"=>claims["iss"]=json!("https://wrong.example"),"expired"=>claims["exp"]=json!(now-300),"iat"=>claims["iat"]=json!(now+300),"nbf"=>claims["nbf"]=json!(now+300),"azp"=>{claims["aud"]=json!(["dashboard","another-client"]);claims["azp"]=json!("another-client");},"multi-no-azp"=>claims["aud"]=json!(["dashboard","another-client"]),"multi-ok"=>{claims["aud"]=json!(["dashboard","another-client"]);claims["azp"]=json!("dashboard");},_=>{} }
            let mut h=Header::new(Algorithm::RS256);h.kid=p.key.jwk["kid"].as_str().map(str::to_owned);
            let token=if p.mode=="none" {format!("{}.{}.",B64.encode(br#"{"alg":"none"}"#),B64.encode(serde_json::to_vec(&claims).unwrap()))} else if p.mode=="hs" { let mut h=Header::new(Algorithm::HS256);h.kid=Some("first".into());jsonwebtoken::encode(&h,&claims,&EncodingKey::from_secret(p.key.public_pem.as_bytes())).unwrap() } else { jsonwebtoken::encode(&h,&claims,&EncodingKey::from_rsa_pem(p.key.pem.as_bytes()).unwrap()).unwrap() };
            (axum::http::StatusCode::OK,Json(json!({"id_token":token,"access_token":"not-retained","token_type":"Bearer"})))
        }))
        .route("/userinfo",get(|State(p):State<Arc<Mutex<Provider>>>,headers:axum::http::HeaderMap|async move {let mut p=p.lock().unwrap();p.userinfo_calls+=1;assert_eq!(headers["authorization"],"Bearer not-retained");Json(json!({"sub":if p.mode=="userinfo-sub" {"wrong-subject"} else {&p.sub},"groups":p.groups}))}))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(l, router).await.unwrap();
    });
    Mock { state, task }
}

#[tokio::test]
async fn oidc_rejects_invalid_tokens_reuse_missing_binding_and_refreshes_rotated_jwks() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, true).await;
    let mut b = Browser::new();
    let first = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&first).await.status(), 200);
    assert_eq!(b.get(&first).await.status(), 401, "state reuse");
    for mode in [
        "nonce",
        "aud",
        "iss",
        "expired",
        "none",
        "hs",
        "iat",
        "nbf",
        "azp",
        "multi-no-azp",
    ] {
        p.state.lock().unwrap().mode = mode.into();
        let callback = begin(&a, &mut b, &p).await;
        let r = b.get(&callback).await;
        assert_eq!(r.status(), 401, "{mode}");
        let text = r.text().await.unwrap();
        assert!(!text.contains("same@example.com"));
        assert!(!text.contains("private-client-secret"));
        assert!(!text.contains("valid-code"));
        assert_eq!(b.get(&callback).await.status(), 401);
    }
    p.state.lock().unwrap().mode.clear();
    let callback = begin(&a, &mut b, &p).await;
    b.cookies.remove("tokomotive_oidc");
    let calls = p.state.lock().unwrap().pkce_calls;
    assert_eq!(b.get(&callback).await.status(), 401);
    assert_eq!(
        p.state.lock().unwrap().pkce_calls,
        calls,
        "missing binding must not send a verifier or code"
    );
    let callback = begin(&a, &mut b, &p).await;
    p.state.lock().unwrap().challenge = "wrong-pkce-challenge".into();
    assert_eq!(
        b.get(&callback).await.status(),
        401,
        "PKCE mismatch at token endpoint"
    );
    let before = p.state.lock().unwrap().jwks_calls;
    p.state.lock().unwrap().key = key("rotated");
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(
        b.get(&callback).await.status(),
        200,
        "unknown kid should refresh JWKS"
    );
    assert_eq!(p.state.lock().unwrap().jwks_calls, before + 1);
    p.state.lock().unwrap().mode = "multi-ok".into();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
}

#[tokio::test]
async fn oidc_jit_off_matches_subject_not_email_and_links_only_with_account_permission() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, true).await;
    let mut b = Browser::new();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    configure(&a, &mut owner, &p, false).await;
    p.state.lock().unwrap().sub = "different-subject-same-email".into();
    let mut unknown = Browser::new();
    let callback = begin(&a, &mut unknown, &p).await;
    assert_eq!(unknown.get(&callback).await.status(), 401);
    assert_eq!(
        unknown.get(&format!("{}/admin", a.url)).await.headers()["location"],
        "/admin/login"
    );
    p.state.lock().unwrap().sub = "subject-one".into();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    let r = b
        .post(
            &format!("{}/admin/oidc/link", a.url),
            &[("account_id", "1")],
        )
        .await;
    assert_eq!(r.status(), 403, "Admin cannot link Owner");
    let r = owner
        .post(
            &format!("{}/admin/oidc/link", a.url),
            &[("account_id", "1")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let u = reqwest::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
    {
        let mut p = p.state.lock().unwrap();
        p.sub = "owner-linked-subject".into();
        p.nonce = q["nonce"].clone();
        p.challenge = q["code_challenge"].clone();
    }
    assert_eq!(
        owner
            .get(&format!(
                "{}/admin/oidc/callback?state={}&code=link-code",
                a.url, q["state"]
            ))
            .await
            .status(),
        200
    );
    let mut linked = Browser::new();
    let callback = begin(&a, &mut linked, &p).await;
    assert_eq!(
        linked.get(&callback).await.status(),
        401,
        "IdP must not demote local Owner"
    );
    let r = linked
        .post(
            &format!("{}/admin/login", a.url),
            &[("username", "chief"), ("password", "owner-password")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let audit = linked
        .get(&format!("{}/admin/audit", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(audit.contains("break-glass"));
}

#[tokio::test]
async fn oidc_owner_mapping_cannot_remove_the_last_local_owner() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    let issuer = p.state.lock().unwrap().base.clone();
    assert_eq!(
        owner
            .post(
                &format!("{}/admin/settings/oidc", a.url),
                &[
                    ("issuer", &issuer),
                    ("public_base_url", &a.url),
                    ("client_id", "dashboard"),
                    ("client_secret", "private-client-secret"),
                    ("scopes", "openid profile email"),
                    ("button_label", "Authentik"),
                    ("enabled", "on"),
                    ("allow_private", "on"),
                    ("jit", "on"),
                    ("default_role", "deny"),
                    ("groups_claim", "groups"),
                    ("group_0", "operators"),
                    ("role_0", "owner"),
                    ("token_auth", "client_secret_basic")
                ]
            )
            .await
            .status(),
        303
    );
    let mut b = Browser::new();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    assert_eq!(
        b.get(&format!("{}/admin/settings/oidc", a.url))
            .await
            .status(),
        200,
        "explicit Owner group grants Owner"
    );
    let r = owner
        .post(&format!("{}/admin/settings/accounts/1/delete", a.url), &[])
        .await;
    assert_eq!(
        r.status(),
        400,
        "retain a password-enabled break-glass Owner even with an OIDC Owner"
    );
}
async fn setup(a: &App) -> Browser {
    let mut b = Browser::new();
    assert_eq!(
        b.post(
            &format!("{}/setup", a.url),
            &[("username", "chief"), ("password", "owner-password")]
        )
        .await
        .status(),
        303
    );
    b
}
async fn configure(a: &App, b: &mut Browser, p: &Mock, jit: bool) {
    let issuer = p.state.lock().unwrap().base.clone();
    let r = b
        .post(
            &format!("{}/admin/settings/oidc", a.url),
            &[
                ("issuer", &issuer),
                ("public_base_url", &a.url),
                ("client_id", "dashboard"),
                ("client_secret", "private-client-secret"),
                ("scopes", "openid profile email"),
                ("button_label", "Authentik"),
                ("enabled", "on"),
                ("allow_private", "on"),
                ("jit", if jit { "on" } else { "" }),
                ("default_role", "deny"),
                ("groups_claim", "groups"),
                ("group_0", "operators"),
                ("role_0", "admin"),
                ("group_1", "readers"),
                ("role_1", "viewer"),
                ("token_auth", "client_secret_basic"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
}
async fn begin(a: &App, b: &mut Browser, _p: &Mock) -> String {
    let r = b.get(&format!("{}/admin/oidc/start", a.url)).await;
    assert_eq!(r.status(), 303);
    let u = reqwest::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let response = b.get(u.as_str()).await;
    assert_eq!(response.status(), 303);
    response.headers()["location"].to_str().unwrap().into()
}

#[tokio::test]
async fn oidc_real_login_maps_groups_changes_roles_and_preserves_local_owner() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, true).await;
    let mut b = Browser::new();
    let html = b
        .get(&format!("{}/admin/login", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(html.contains("Sign in with Authentik"));
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    let html = b
        .get(&format!("{}/admin/settings", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(html.contains("Change password"));
    assert_eq!(
        b.get(&format!("{}/admin/settings/oidc", a.url))
            .await
            .status(),
        403
    );
    assert_eq!(
        b.post(&format!("{}/admin/settings/oidc", a.url), &[])
            .await
            .status(),
        403
    );
    p.state.lock().unwrap().groups = vec!["readers".into()];
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    assert_eq!(
        b.post(&format!("{}/admin/users", a.url), &[("name", "denied")])
            .await
            .status(),
        403
    );
    let mut local = Browser::new();
    assert_eq!(
        local
            .post(
                &format!("{}/admin/login", a.url),
                &[("username", "chief"), ("password", "owner-password")]
            )
            .await
            .status(),
        303
    );
    let html = owner
        .get(&format!("{}/admin/settings/oidc", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(!html.contains("private-client-secret"));
    assert!(html.contains(&format!("{}/admin/oidc/callback", a.url)));
    let audit = owner
        .get(&format!("{}/admin/audit", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(audit.contains("OIDC role changed at sign-in"));
    assert!(audit.contains("Local password login succeeded"));
    assert!(!audit.contains("private-client-secret"));
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&a.st.db)
        .await
        .unwrap();
    for name in ["db.sqlite", "db.sqlite-wal"] {
        if let Ok(bytes) = std::fs::read(a._dir.path().join(name)) {
            assert!(!bytes.windows(21).any(|v| v == b"private-client-secret"));
        }
    }
    assert_eq!(p.state.lock().unwrap().pkce_calls, 2);
}

#[tokio::test]
async fn oidc_userinfo_requires_matching_subject_and_supports_client_secret_post() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    let issuer = p.state.lock().unwrap().base.clone();
    assert_eq!(
        owner
            .post(
                &format!("{}/admin/settings/oidc", a.url),
                &[
                    ("issuer", &issuer),
                    ("public_base_url", &a.url),
                    ("client_id", "dashboard"),
                    ("client_secret", "private-client-secret"),
                    ("scopes", "openid profile email"),
                    ("button_label", "Authentik"),
                    ("enabled", "on"),
                    ("allow_private", "on"),
                    ("jit", "on"),
                    ("default_role", "deny"),
                    ("groups_claim", "groups"),
                    ("group_0", "operators"),
                    ("role_0", "admin"),
                    ("token_auth", "client_secret_post"),
                    ("userinfo", "on")
                ]
            )
            .await
            .status(),
        303
    );
    p.state.lock().unwrap().post_auth = true;
    let mut b = Browser::new();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 200);
    p.state.lock().unwrap().mode = "userinfo-sub".into();
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(b.get(&callback).await.status(), 401);
    assert_eq!(p.state.lock().unwrap().userinfo_calls, 2);
    p.state.lock().unwrap().mode.clear();
    p.state.lock().unwrap().groups = vec!["not-mapped".into()];
    let callback = begin(&a, &mut b, &p).await;
    assert_eq!(
        b.get(&callback).await.status(),
        401,
        "default must deny; never infer Owner"
    );
}

#[tokio::test]
async fn oidc_link_requires_the_initiating_session_to_still_exist() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, false).await;
    let r = owner
        .post(
            &format!("{}/admin/oidc/link", a.url),
            &[("account_id", "1")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let u = reqwest::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
    {
        let mut p = p.state.lock().unwrap();
        p.nonce = q["nonce"].clone();
        p.challenge = q["code_challenge"].clone();
    }
    assert_eq!(
        owner
            .post(&format!("{}/admin/logout", a.url), &[])
            .await
            .status(),
        303
    );
    let callback = format!(
        "{}/admin/oidc/callback?state={}&code=link-code",
        a.url, q["state"]
    );
    assert_eq!(owner.get(&callback).await.status(), 401);
}

#[tokio::test]
async fn oidc_discovery_denies_redirects_large_responses_and_unapproved_http() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    let issuer = p.state.lock().unwrap().base.clone();
    for mode in [
        "private-without-opt-in",
        "discovery-redirect",
        "discovery-large",
        "discovery-issuer",
        "discovery-public-http",
    ] {
        p.state.lock().unwrap().mode = mode.into();
        let response = owner
            .post(
                &format!("{}/admin/settings/oidc", a.url),
                &[
                    ("issuer", &issuer),
                    ("public_base_url", &a.url),
                    ("client_id", "dashboard"),
                    ("client_secret", "private-client-secret"),
                    ("scopes", "openid profile email"),
                    ("button_label", "Authentik"),
                    ("enabled", "on"),
                    (
                        "allow_private",
                        if mode == "private-without-opt-in" {
                            ""
                        } else {
                            "on"
                        },
                    ),
                    ("default_role", "deny"),
                    ("groups_claim", "groups"),
                    ("token_auth", "client_secret_basic"),
                ],
            )
            .await;
        assert_eq!(response.status(), 400, "{mode}");
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains("private-client-secret")
        );
    }
    assert_eq!(
        p.state.lock().unwrap().userinfo_calls,
        0,
        "must not follow the other-host redirect"
    );
    let mut b = Browser::new();
    assert_eq!(
        b.get(&format!("{}/admin/oidc/start", a.url)).await.status(),
        401,
        "failed settings must not enable SSO"
    );
}

#[tokio::test]
async fn oidc_viewer_can_link_self_and_owner_can_link_another_local_account() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, false).await;
    assert_eq!(
        owner
            .post(
                &format!("{}/admin/settings/accounts", a.url),
                &[
                    ("username", "reader"),
                    ("role", "viewer"),
                    ("password", "reader-password")
                ]
            )
            .await
            .status(),
        303
    );
    let mut viewer = Browser::new();
    assert_eq!(
        viewer
            .post(
                &format!("{}/admin/login", a.url),
                &[("username", "reader"), ("password", "reader-password")]
            )
            .await
            .status(),
        303
    );
    let html = viewer
        .get(&format!("{}/admin/account", a.url))
        .await
        .text()
        .await
        .unwrap();
    assert!(html.contains("action=\"/admin/oidc/link\""));
    p.state.lock().unwrap().groups = vec!["readers".into()];
    for (browser, sub) in [
        (&mut viewer, "viewer-self"),
        (&mut owner, "owner-linked-reader"),
    ] {
        p.state.lock().unwrap().sub = sub.into();
        let response = browser
            .post(
                &format!("{}/admin/oidc/link", a.url),
                &[("account_id", "2")],
            )
            .await;
        assert_eq!(response.status(), 303);
        let authorization = response.headers()["location"].to_str().unwrap().to_owned();
        let response = browser.get(&authorization).await;
        assert_eq!(response.status(), 303);
        let callback = response.headers()["location"].to_str().unwrap().to_owned();
        assert_eq!(browser.get(&callback).await.status(), 200);
        let mut login = Browser::new();
        let callback = begin(&a, &mut login, &p).await;
        assert_eq!(login.get(&callback).await.status(), 200);
        let html = login
            .get(&format!("{}/admin/account", a.url))
            .await
            .text()
            .await
            .unwrap();
        assert!(html.contains("<h2 id=\"account-h\">reader</h2>"));
    }
}

#[tokio::test]
async fn oidc_callback_keeps_strict_session_and_completes_on_the_same_origin() {
    let a = app().await;
    let p = mock().await;
    let mut owner = setup(&a).await;
    configure(&a, &mut owner, &p, true).await;
    let mut browser = Browser::new();
    let start = browser.get(&format!("{}/admin/oidc/start", a.url)).await;
    let binding = start.headers()["set-cookie"].to_str().unwrap();
    for part in [
        "tokomotive_oidc=",
        "HttpOnly",
        "SameSite=Lax",
        "Path=/admin/oidc",
        "Max-Age=600",
    ] {
        assert!(binding.contains(part), "{part}");
    }
    assert!(
        !binding.contains("; Secure"),
        "private HTTP fixture must not use Secure"
    );
    let authorization = start.headers()["location"].to_str().unwrap().to_owned();
    let response = browser.get(&authorization).await;
    let callback = response.headers()["location"].to_str().unwrap().to_owned();
    let response = browser.get(&callback).await;
    assert_eq!(response.status(), 200);
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap())
        .collect::<Vec<_>>();
    assert!(cookies.iter().any(|h| h.starts_with("tokomotive_session=")
        && h.contains("HttpOnly")
        && h.contains("SameSite=Strict")
        && h.contains("Path=/admin;")));
    assert!(
        cookies
            .iter()
            .any(|h| h.starts_with("tokomotive_oidc=") && h.contains("Max-Age=0"))
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    let html = response.text().await.unwrap();
    assert!(html.contains("http-equiv=\"refresh\""));
    assert!(html.contains("url=/admin"));
    assert!(html.contains("href=\"/admin\""));
    assert!(!html.contains("<script"));
    assert!(!html.contains("valid-code"));
    assert_eq!(browser.get(&format!("{}/admin", a.url)).await.status(), 200);
    let issuer = p.state.lock().unwrap().base.clone();
    let https_base = a.url.replacen("http:", "HTTPS:", 1);
    assert_eq!(
        owner
            .post(
                &format!("{}/admin/settings/oidc", a.url),
                &[
                    ("issuer", &issuer),
                    ("public_base_url", &https_base),
                    ("client_id", "dashboard"),
                    ("client_secret", ""),
                    ("scopes", "openid profile email"),
                    ("button_label", "Authentik"),
                    ("enabled", "on"),
                    ("allow_private", "on"),
                    ("jit", "on"),
                    ("default_role", "deny"),
                    ("groups_claim", "groups"),
                    ("group_0", "operators"),
                    ("role_0", "admin"),
                    ("token_auth", "client_secret_basic")
                ]
            )
            .await
            .status(),
        303
    );
    let response = browser.get(&format!("{}/admin/oidc/start", a.url)).await;
    assert!(
        response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
}
