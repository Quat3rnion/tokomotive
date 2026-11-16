//! Optional dashboard OIDC. Credentials and protocol artifacts never enter views or audit snapshots.
use super::*;
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation,
    jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const PRELOGIN_COOKIE: &str = "tokomotive_oidc";
const SETTINGS: &str = "dashboard_oidc";
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
struct Config {
    issuer: String,
    public_base_url: String,
    client_id: String,
    client_secret: String,
    scopes: String,
    button_label: String,
    enabled: bool,
    allow_private: bool,
    token_auth: String,
    groups_claim: String,
    mappings: Vec<Mapping>,
    jit: bool,
    default_role: String,
    userinfo: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            public_base_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            scopes: "openid profile email".into(),
            button_label: "Authentik".into(),
            enabled: false,
            allow_private: false,
            token_auth: "client_secret_basic".into(),
            groups_claim: "groups".into(),
            mappings: Vec::new(),
            jit: false,
            default_role: "deny".into(),
            userinfo: false,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
struct Mapping {
    group: String,
    role: String,
}
impl Config {
    fn redirect_uri(&self) -> String {
        format!(
            "{}/admin/oidc/callback",
            self.public_base_url.trim_end_matches('/')
        )
    }
    fn fingerprint(&self) -> String {
        db::hash_key(&serde_json::to_string(self).expect("OIDC configuration"))
    }
}
async fn config(st: &AppState) -> Result<Config> {
    crate::settings::value(&st.db, SETTINGS)
        .await?
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map(|c| c.unwrap_or_default())
        .map_err(Into::into)
}
pub(super) async fn label(st: &AppState) -> Result<Option<String>> {
    let c = config(st).await?;
    Ok(c.enabled.then_some(c.button_label))
}

#[derive(Default)]
pub(crate) struct Runtime {
    pending: Mutex<HashMap<String, Pending>>,
    initiations: Mutex<InitiationLimiter>,
    discovery: Mutex<Option<CachedDiscovery>>,
    jwks: tokio::sync::Mutex<Option<CachedKeys>>,
}
#[derive(Default)]
struct InitiationLimiter {
    ips: super::accounts::LoginLimiter,
    accounts: HashMap<i64, (i64, u8)>,
}
impl InitiationLimiter {
    fn allow(&mut self, ip: &str, account: Option<i64>, now: i64) -> bool {
        self.accounts.retain(|_, (start, _)| now - *start < 300);
        if account.is_some_and(|id| self.accounts.get(&id).is_some_and(|(_, n)| *n >= 5)) {
            return false;
        }
        if !self.ips.allow(ip, "", now) {
            return false;
        }
        if let Some(id) = account {
            if !self.accounts.contains_key(&id)
                && self.accounts.len() >= 4096
                && let Some(oldest) = self
                    .accounts
                    .iter()
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(id, _)| *id)
            {
                self.accounts.remove(&oldest);
            }
            self.accounts.entry(id).or_insert((now, 0)).1 += 1;
        }
        true
    }
}
struct CachedDiscovery {
    config: String,
    fetched: Instant,
    discovery: Discovery,
}
struct Pending {
    expires: i64,
    binding: String,
    nonce: String,
    verifier: String,
    config: String,
    discovery: Discovery,
    link: Option<Link>,
}
struct Link {
    actor: i64,
    version: i64,
    account: i64,
    session_hash: String,
}
struct CachedKeys {
    uri: String,
    fetched: Instant,
    keys: JwkSet,
}
#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    userinfo_endpoint: Option<String>,
}
fn url(value: &str) -> Result<reqwest::Url> {
    let u = reqwest::Url::parse(value).context("invalid OIDC URL")?;
    ensure!(
        matches!(u.scheme(), "https" | "http")
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
            && value.len() <= 8192,
        "invalid OIDC URL"
    );
    Ok(u)
}
async fn client(c: &Config, target: &str) -> Result<(reqwest::Client, reqwest::Url)> {
    let u = url(target)?;
    let client = crate::secrets::adapters::direct_client(&u, c.allow_private).await?;
    Ok((client, u))
}
async fn receive<T: DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T> {
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OIDC request failed"))?;
    ensure!(
        response.status().is_success(),
        "OIDC endpoint rejected request ({})",
        response.status()
    );
    ensure!(
        response.content_length().is_none_or(|n| n <= 1024 * 1024),
        "OIDC response too large"
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("OIDC response failed"))?;
        ensure!(
            bytes.len() + chunk.len() <= 1024 * 1024,
            "OIDC response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid OIDC JSON response"))
}
async fn discover(c: &Config) -> Result<Discovery> {
    let (http, u) = client(
        c,
        &format!(
            "{}/.well-known/openid-configuration",
            c.issuer.trim_end_matches('/')
        ),
    )
    .await?;
    let d: Discovery = receive(http.get(u)).await?;
    ensure!(d.issuer == c.issuer, "OIDC discovery issuer mismatch");
    for endpoint in [&d.authorization_endpoint, &d.token_endpoint, &d.jwks_uri]
        .into_iter()
        .chain(d.userinfo_endpoint.as_ref())
    {
        client(c, endpoint).await?;
    }
    Ok(d)
}
async fn cached_discovery(st: &AppState, c: &Config) -> Result<Discovery> {
    let fingerprint = c.fingerprint();
    if let Some(cached) = st
        .dashboard
        .oidc
        .discovery
        .lock()
        .expect("OIDC discovery")
        .as_ref()
        && cached.config == fingerprint
        && cached.fetched.elapsed() < Duration::from_secs(60)
    {
        return Ok(cached.discovery.clone());
    }
    // Never hold either the cache mutex or the dashboard mutation lock over DNS/HTTP.
    let discovery = discover(c).await?;
    *st.dashboard.oidc.discovery.lock().expect("OIDC discovery") = Some(CachedDiscovery {
        config: fingerprint,
        fetched: Instant::now(),
        discovery: discovery.clone(),
    });
    Ok(discovery)
}
fn random() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}
fn prelogin_cookie(c: &Config, value: &str, age: u16) -> String {
    format!(
        "{PRELOGIN_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/admin/oidc; Max-Age={age}{}",
        if reqwest::Url::parse(&c.public_base_url).is_ok_and(|u| u.scheme() == "https") {
            "; Secure"
        } else {
            ""
        }
    )
}
pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/oidc", get(page).post(save))
        .route("/admin/oidc/start", get(start))
        .route("/admin/oidc/callback", get(callback))
        .route("/admin/oidc/link", post(link))
        .route("/admin/oidc/roles", get(roles))
        .route("/admin/account", get(account_page))
}
#[derive(Template)]
#[template(path = "oidc.html")]
struct Page {
    base: Base,
    c: Config,
    redirect_uri: String,
    secret_set: bool,
}
async fn page(_: Admin, State(st): State<AppState>) -> Response {
    let mut c = tryr!(config(&st).await);
    let secret_set = !c.client_secret.is_empty();
    c.client_secret.clear();
    render(Page {
        base: Base::new(&st),
        redirect_uri: c.redirect_uri(),
        c,
        secret_set,
    })
}
fn role(value: &str) -> bool {
    matches!(value, "owner" | "admin" | "viewer")
}
fn on(f: &HashMap<String, String>, key: &str) -> bool {
    f.get(key).is_some_and(|s| s == "on")
}
async fn save(
    Admin(actor): Admin,
    State(st): State<AppState>,
    Form(f): Form<HashMap<String, String>>,
) -> Response {
    let previous = tryr!(config(&st).await);
    let get = |k: &str| f.get(k).cloned().unwrap_or_default().trim().to_owned();
    let mut c = Config {
        issuer: get("issuer"),
        public_base_url: get("public_base_url").trim_end_matches('/').into(),
        client_id: get("client_id"),
        client_secret: previous.client_secret.clone(),
        scopes: get("scopes"),
        button_label: get("button_label"),
        enabled: on(&f, "enabled"),
        allow_private: on(&f, "allow_private"),
        token_auth: get("token_auth"),
        groups_claim: get("groups_claim"),
        jit: on(&f, "jit"),
        default_role: get("default_role"),
        userinfo: on(&f, "userinfo"),
        mappings: Vec::new(),
    };
    for n in 0..64 {
        let group = get(&format!("group_{n}"));
        let r = get(&format!("role_{n}"));
        if !group.is_empty() {
            if group.len() > 256 || !role(&r) || c.mappings.iter().any(|m| m.group == group) {
                return bad("Use a unique group name and choose Owner, Admin or Viewer.");
            }
            c.mappings.push(Mapping { group, role: r });
        }
    }
    if !matches!(
        c.token_auth.as_str(),
        "client_secret_basic" | "client_secret_post"
    ) || !matches!(c.default_role.as_str(), "deny" | "admin" | "viewer")
        || c.groups_claim.is_empty()
        || c.groups_claim.len() > 128
        || c.button_label.is_empty()
        || c.button_label.len() > 80
        || c.scopes.len() > 1024
        || !c.scopes.split_whitespace().any(|s| s == "openid")
    {
        return bad(
            "Choose token authentication, a default role, a groups claim, a button label, and scopes that include openid.",
        );
    }
    if !get("client_secret").is_empty() {
        c.client_secret = tryr!(st.secret_key.encrypt(&get("client_secret")));
    }
    if c.enabled
        && (c.client_id.is_empty() || c.client_id.len() > 512 || c.client_secret.is_empty())
    {
        return bad("Set the client ID and client secret before you enable Single sign-on.");
    }
    if !c.public_base_url.is_empty() {
        let u = tryform!(url(&c.public_base_url));
        if !matches!(u.path(), "" | "/") || u.query().is_some() {
            return bad("Use the public base URL without a path or query.");
        }
        if let Err(e) = client(&c, &c.public_base_url).await {
            return form_error(e);
        }
    } else if c.enabled {
        return bad("Set the public base URL.");
    }
    if !c.issuer.is_empty() {
        let u = tryform!(url(&c.issuer));
        if u.query().is_some() {
            return bad("The issuer URL cannot have a query.");
        }
        if c.enabled
            && let Err(e) = discover(&c).await
        {
            tracing::warn!(error=%e,"OIDC discovery validation failed");
            return bad("OIDC discovery failed. Check the issuer and network settings.");
        }
    } else if c.enabled {
        return bad("Set the issuer URL.");
    }
    tryr!(crate::settings::put(&st.db, SETTINGS, &tryr!(serde_json::to_string(&c))).await);
    st.dashboard
        .oidc
        .pending
        .lock()
        .expect("OIDC pending")
        .clear();
    *st.dashboard.oidc.jwks.lock().await = None;
    *st.dashboard.oidc.discovery.lock().expect("OIDC discovery") = None;
    tryr!(audit(&st,&actor,"oidc.settings","Single sign-on settings saved.",json!({}),json!({"enabled":c.enabled,"jit":c.jit,"default_role":c.default_role,"mappings":c.mappings})).await);
    saved("/admin/settings/oidc", "Single sign-on settings saved.")
}
fn failed(error: anyhow::Error) -> Response {
    // Error contexts below are static, or library validation codes. No response bodies or claims.
    tracing::warn!(error=%format!("{error:#}"),"OIDC sign-in failed");
    page_error(
        StatusCode::UNAUTHORIZED,
        "Single sign-on failed. Try again or use the local login form.",
    )
}
type Peer = std::result::Result<
    axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::rejection::ExtensionRejection,
>;
fn initiation_allowed(st: &AppState, peer: Peer, account: Option<i64>) -> bool {
    let ip = peer
        .ok()
        .map(|p| p.0.ip().to_string())
        .unwrap_or_else(|| "local".into());
    st.dashboard
        .oidc
        .initiations
        .lock()
        .expect("OIDC initiation limiter")
        .allow(&ip, account, db::now_s())
}
fn limited() -> Response {
    let mut response = page_error(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many login attempts. Wait 5 minutes.",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("300"));
    response
}
async fn start(State(st): State<AppState>, peer: Peer, headers: HeaderMap) -> Response {
    if !initiation_allowed(&st, peer, None) {
        return limited();
    }
    match begin(&st, &headers, None).await {
        Ok(r) => r,
        Err(e) => failed(e),
    }
}
async fn begin(st: &AppState, headers: &HeaderMap, link: Option<Link>) -> Result<Response> {
    let c = config(st).await?;
    ensure!(c.enabled, "OIDC disabled");
    let d = cached_discovery(st, &c).await?;
    let _guard = st.dashboard.mutations.lock().await;
    ensure!(
        config(st).await?.fingerprint() == c.fingerprint(),
        "OIDC settings changed during initiation"
    );
    if let Some(link) = &link {
        link_actor(st, link).await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_accounts WHERE id=?)")
                .bind(link.account)
                .fetch_one(&st.db)
                .await?;
        ensure!(exists, "OIDC link target no longer exists");
    }
    let state = random();
    let binding = cookie(headers, PRELOGIN_COOKIE)
        .filter(|s| {
            s.len() == 43
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .unwrap_or_else(random);
    let binding_hash = db::hash_key(&binding);
    let nonce = random();
    let verifier = random();
    let mut authorize = url(&d.authorization_endpoint)?;
    authorize.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", &c.client_id),
        ("redirect_uri", &c.redirect_uri()),
        ("scope", &c.scopes),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge_method", "S256"),
        (
            "code_challenge",
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes())),
        ),
    ]);
    let mut pending = st.dashboard.oidc.pending.lock().expect("OIDC pending");
    pending.retain(|_, p| {
        p.expires > db::now_s()
            && p.binding != binding_hash
            && !link.as_ref().is_some_and(|new| {
                p.link
                    .as_ref()
                    .is_some_and(|old| old.actor == new.actor || old.account == new.account)
            })
    });
    ensure!(pending.len() < 4096, "OIDC login capacity reached");
    pending.insert(
        state,
        Pending {
            expires: db::now_s() + 600,
            binding: binding_hash,
            nonce,
            verifier,
            config: c.fingerprint(),
            discovery: d,
            link,
        },
    );
    Ok((
        [(header::SET_COOKIE, prelogin_cookie(&c, &binding, 600))],
        Redirect::to(authorize.as_str()),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
}
async fn callback(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let mut response = match finish(&st, &headers, q).await {
        Ok(r) => r,
        Err(e) => failed(e),
    };
    let c = config(&st).await.unwrap_or_default();
    if let Ok(value) = HeaderValue::from_str(&prelogin_cookie(&c, "", 0)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// A new same-origin navigation sends Strict cookies after the IdP redirect chain.
fn complete(target: &'static str) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer")],
        Html(format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0;url={target}\"><title>Sign-in complete - Tokomotive</title></head><body><h1>Sign-in complete</h1><p><a href=\"{target}\">Continue to Tokomotive</a></p></body></html>")),
    ).into_response()
}
async fn finish(st: &AppState, headers: &HeaderMap, q: Callback) -> Result<Response> {
    let state = q.state.context("missing OIDC state")?;
    let p = st
        .dashboard
        .oidc
        .pending
        .lock()
        .expect("OIDC pending")
        .remove(&state)
        .context("unknown or reused OIDC state")?;
    let c = config(st).await?;
    ensure!(
        p.expires > db::now_s() && c.enabled && p.config == c.fingerprint(),
        "expired or changed OIDC login"
    );
    let binding = cookie(headers, PRELOGIN_COOKIE).context("missing OIDC browser binding")?;
    ensure!(
        ct_eq(&p.binding, &db::hash_key(&binding)),
        "OIDC browser binding mismatch"
    );
    ensure!(p.verifier.len() >= 43, "missing PKCE verifier");
    let code = q.code.context("missing OIDC authorization code")?;
    ensure!(code.len() <= 8192, "OIDC code too large");
    let (http, u) = client(&c, &p.discovery.token_endpoint).await?;
    let secret = st
        .secret_key
        .decrypt(&c.client_secret)
        .context("OIDC secret unavailable")?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &c.redirect_uri()),
        ("code_verifier", &p.verifier),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect::<Vec<_>>();
    let mut request = http.post(u);
    if c.token_auth == "client_secret_post" {
        form.push(("client_id".into(), c.client_id.clone()));
        form.push(("client_secret".into(), secret));
    } else {
        // OAuth Basic credentials use form-url-encoding before Base64 (RFC 6749 2.3.1).
        let encode = |s: &str| {
            serde_urlencoded::to_string([("", s)])
                .expect("form encoding")
                .trim_start_matches('=')
                .to_owned()
        };
        request = request.basic_auth(encode(&c.client_id), Some(encode(&secret)));
    }
    #[derive(Deserialize)]
    struct Tokens {
        id_token: String,
        access_token: Option<String>,
    }
    let tokens: Tokens = receive(request.form(&form)).await?;
    let mut claims = validate(st, &c, &p.discovery, &tokens.id_token, &p.nonce).await?;
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .context("missing OIDC subject")?
        .to_owned();
    ensure!(!sub.is_empty() && sub.len() <= 1024, "invalid OIDC subject");
    if c.userinfo {
        let endpoint = p
            .discovery
            .userinfo_endpoint
            .as_ref()
            .context("userinfo endpoint unavailable")?;
        let token = tokens
            .access_token
            .context("userinfo access token unavailable")?;
        let (http, u) = client(&c, endpoint).await?;
        let info: Value = receive(http.get(u).bearer_auth(token)).await?;
        ensure!(
            info.get("sub").and_then(Value::as_str) == Some(sub.as_str()),
            "userinfo subject mismatch"
        );
        // Only the configured groups claim is supplemented; never replace ID token identity claims.
        if let Some(groups) = info.get(&c.groups_claim) {
            claims[&c.groups_claim] = groups.clone();
        }
    }
    let _guard = st.dashboard.mutations.lock().await;
    // Recheck after the network calls: an Owner can disable or edit SSO while a request is in flight.
    ensure!(
        config(st).await?.fingerprint() == p.config,
        "OIDC settings changed during login"
    );
    if let Some(link) = p.link {
        let actor = link_actor(st, &link).await?;
        sqlx::query("INSERT INTO admin_oidc_identities(issuer,subject,account_id) VALUES(?,?,?)")
            .bind(&c.issuer)
            .bind(&sub)
            .bind(link.account)
            .execute(&st.db)
            .await
            .map_err(|_| anyhow::anyhow!("identity already linked or account unavailable"))?;
        audit(
            st,
            &actor,
            "oidc.link",
            "OIDC identity linked.",
            json!({}),
            json!({"account_id":link.account}),
        )
        .await?;
        return Ok(complete("/admin/account"));
    }
    let account = map_account(st, &c, &claims, &sub).await?;
    let session = start_session(st, &account).await?;
    audit(
        st,
        &account,
        "login",
        "OIDC login succeeded.",
        json!({}),
        json!({"method":"oidc"}),
    )
    .await?;
    let mut response = complete("/admin");
    response
        .headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(&session)?);
    Ok(response)
}
async fn validate(
    st: &AppState,
    c: &Config,
    d: &Discovery,
    token: &str,
    nonce: &str,
) -> Result<Value> {
    let h = jsonwebtoken::decode_header(token).context("invalid OIDC token header")?;
    ensure!(
        matches!(
            h.alg,
            Algorithm::RS256 | Algorithm::ES256 | Algorithm::EdDSA
        ),
        "OIDC token algorithm denied"
    );
    let kid = h.kid.context("missing OIDC key ID")?;
    ensure!(kid.len() <= 256, "invalid OIDC key ID");
    let mut cache = st.dashboard.oidc.jwks.lock().await;
    let fresh = cache
        .as_ref()
        .is_some_and(|k| k.uri == d.jwks_uri && k.fetched.elapsed() < Duration::from_secs(300));
    if !fresh || cache.as_ref().is_none_or(|k| k.keys.find(&kid).is_none()) {
        let (http, u) = client(c, &d.jwks_uri).await?;
        *cache = Some(CachedKeys {
            uri: d.jwks_uri.clone(),
            fetched: Instant::now(),
            keys: receive(http.get(u)).await?,
        });
    }
    let jwk = cache
        .as_ref()
        .and_then(|k| k.keys.find(&kid))
        .context("unknown OIDC key ID")?;
    ensure!(
        !matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)),
        "symmetric OIDC key denied"
    );
    ensure!(
        jwk.common
            .public_key_use
            .as_ref()
            .is_none_or(|u| *u == PublicKeyUse::Signature)
            && jwk
                .common
                .key_operations
                .as_ref()
                .is_none_or(|ops| ops.contains(&KeyOperations::Verify)),
        "OIDC key is not for signature verification"
    );
    ensure!(
        jwk.common
            .key_algorithm
            .is_none_or(|alg| alg.to_string() == format!("{:?}", h.alg)),
        "OIDC key algorithm mismatch"
    );
    let key = DecodingKey::from_jwk(jwk).context("invalid OIDC public key")?;
    drop(cache);
    let mut validation = Validation::new(h.alg);
    validation.set_issuer(&[&c.issuer]);
    validation.set_audience(&[&c.client_id]);
    validation.leeway = 30;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["iss", "sub", "aud", "exp", "iat", "nonce"]);
    let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
        .context("OIDC token validation failed")?
        .claims;
    let now = db::now_s();
    let iat = claims
        .get("iat")
        .and_then(Value::as_i64)
        .context("missing OIDC issued-at time")?;
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .context("missing OIDC expiry")?;
    ensure!(
        iat <= now + 30 && iat <= exp && iat >= now - 630,
        "OIDC token issued-at time denied"
    );
    if let Some(nbf) = claims.get("nbf") {
        ensure!(
            nbf.as_i64().is_some_and(|n| n <= now + 30),
            "OIDC not-before time denied"
        );
    }
    ensure!(
        claims
            .get("nonce")
            .and_then(Value::as_str)
            .is_some_and(|n| ct_eq(n, nonce)),
        "OIDC nonce mismatch"
    );
    let multiple = claims
        .get("aud")
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() > 1);
    if multiple || claims.get("azp").is_some() {
        ensure!(
            claims.get("azp").and_then(Value::as_str) == Some(c.client_id.as_str()),
            "OIDC authorized party mismatch"
        );
    }
    Ok(claims)
}
async fn map_account(
    st: &AppState,
    c: &Config,
    claims: &Value,
    sub: &str,
) -> Result<crate::admin::Account> {
    let groups = claims.get(&c.groups_claim).and_then(Value::as_array);
    // Prefer the strongest explicitly mapped group. The default cannot grant Owner.
    let rank = |r: &str| match r {
        "owner" => 3,
        "admin" => 2,
        "viewer" => 1,
        _ => 0,
    };
    let mapped = c
        .mappings
        .iter()
        .filter(|m| groups.is_some_and(|g| g.iter().any(|g| g.as_str() == Some(m.group.as_str()))))
        .max_by_key(|m| rank(&m.role))
        .map(|m| m.role.as_str());
    let role = mapped.unwrap_or(&c.default_role);
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT account_id FROM admin_oidc_identities WHERE issuer=? AND subject=?",
    )
    .bind(&c.issuer)
    .bind(sub)
    .fetch_optional(&st.db)
    .await?;
    ensure!(role != "deny", "no OIDC role mapping");
    let id = if let Some(id) = id {
        id
    } else {
        ensure!(c.jit, "OIDC account not linked and JIT disabled");
        let username = format!(
            "oidc-{}",
            &db::hash_key(&format!("{}\0{sub}", c.issuer))[..32]
        );
        let hash = &st.dashboard.dummy_hash;
        let mut tx = crate::db::begin_write(&st.db, "oidc_jit").await?;
        let id = sqlx::query(
            "INSERT INTO admin_accounts(username,password_hash,role,local_login) VALUES(?,?,?,0)",
        )
        .bind(username)
        .bind(hash)
        .bind(role)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query("INSERT INTO admin_oidc_identities(issuer,subject,account_id) VALUES(?,?,?)")
            .bind(&c.issuer)
            .bind(sub)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        id
    };
    let mut account: crate::admin::Account = sqlx::query_as(
        "SELECT id,username,role,password_hash,session_version FROM admin_accounts WHERE id=?",
    )
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    if account.role != role {
        let local: bool = sqlx::query_scalar("SELECT local_login FROM admin_accounts WHERE id=?")
            .bind(id)
            .fetch_one(&st.db)
            .await?;
        if local && account.role == "owner" {
            audit(
                st,
                &account,
                "oidc.role.denied",
                "OIDC role change denied for the local break-glass Owner.",
                json!({"role":account.role}),
                json!({"requested_role":role}),
            )
            .await?;
            anyhow::bail!("OIDC cannot demote the local break-glass Owner");
        }
        let before = account.role.clone();
        sqlx::query("UPDATE admin_accounts SET role=? WHERE id=?")
            .bind(role)
            .bind(id)
            .execute(&st.db)
            .await?;
        account = sqlx::query_as(
            "SELECT id,username,role,password_hash,session_version FROM admin_accounts WHERE id=?",
        )
        .bind(id)
        .fetch_one(&st.db)
        .await?;
        audit(
            st,
            &account,
            "oidc.role",
            "OIDC role changed at sign-in.",
            json!({"role":before}),
            json!({"role":role}),
        )
        .await?;
    }
    Ok(account)
}
async fn audit(
    st: &AppState,
    account: &crate::admin::Account,
    action: &str,
    summary: &str,
    before: Value,
    after: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO admin_audit(account_id,account,action,target_type,target_id,summary,before_json,after_json) VALUES(?,?,?,'oidc',?,?,?,?)").bind(account.id).bind(&account.username).bind(action).bind(account.id.to_string()).bind(summary).bind(before.to_string()).bind(after.to_string()).execute(&st.db).await?;
    Ok(())
}
#[derive(Deserialize)]
struct LinkForm {
    account_id: i64,
}
async fn link_actor(st: &AppState, link: &Link) -> Result<crate::admin::Account> {
    let actor:crate::admin::Account=sqlx::query_as("SELECT a.id,a.username,a.role,a.password_hash,a.session_version FROM admin_accounts a JOIN sessions s ON s.account_id=a.id WHERE a.id=? AND a.session_version=? AND s.token_hash=? AND s.account_version=a.session_version AND s.password_hash=a.password_hash AND s.expires_at>=?").bind(link.actor).bind(link.version).bind(&link.session_hash).bind(db::now_s()).fetch_optional(&st.db).await?.context("OIDC link initiator session ended or changed")?;
    ensure!(
        actor.id == link.account || actor.role == "owner",
        "OIDC link permission changed"
    );
    Ok(actor)
}
async fn link(
    Admin(actor): Admin,
    State(st): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Form(f): Form<LinkForm>,
) -> Response {
    if actor.id != f.account_id && actor.role != "owner" {
        return page_error(StatusCode::FORBIDDEN, "You can link only your own account.");
    }
    if !initiation_allowed(&st, peer, Some(actor.id)) {
        return limited();
    }
    let exists: bool = tryr!(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_accounts WHERE id=?)")
            .bind(f.account_id)
            .fetch_one(&st.db)
            .await
    );
    if !exists {
        return bad("Account not found.");
    }
    match begin(
        &st,
        &headers,
        Some(Link {
            actor: actor.id,
            version: actor.session_version,
            account: f.account_id,
            session_hash: db::hash_key(&session_cookie(&headers).unwrap_or_default()),
        }),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => failed(e),
    }
}
#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    base: Base,
    id: i64,
    username: String,
    enabled: bool,
    linked: i64,
}
async fn account_page(Admin(account): Admin, State(st): State<AppState>) -> Response {
    let c = tryr!(config(&st).await);
    let linked = tryr!(
        sqlx::query_scalar("SELECT count(*) FROM admin_oidc_identities WHERE account_id=?")
            .bind(account.id)
            .fetch_one(&st.db)
            .await
    );
    render(AccountPage {
        base: Base::new(&st),
        id: account.id,
        username: account.username,
        enabled: c.enabled,
        linked,
    })
}
async fn roles(_: Admin, Query(q): Query<HashMap<String, String>>) -> Html<String> {
    let query = q
        .get("q")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    Html([("owner","Owner"),("admin","Admin"),("viewer","Viewer")].into_iter().filter(|(value,_)|value.contains(&query)).map(|(value,label)|format!("<li role=\"option\" class=\"cb-opt\" data-value=\"{value}\"><span class=\"cb-name\">{label}</span></li>")).collect())
}
