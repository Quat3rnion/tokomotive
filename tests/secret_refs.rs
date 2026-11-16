//! Secret references through the dashboard and the real Proxy.
use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use tokomotive::{config::Config, db, state::AppState};

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
}
impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str("").unwrap();
        cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
        let st = tokomotive::init(&cfg, None).await.unwrap();
        tokomotive::settings::put(&st.db, "admin_password_hash", "fixture-hash")
            .await
            .unwrap();
        db::create_session(&st.db, "secrets-owner", 1)
            .await
            .unwrap();
        let url = serve(tokomotive::router(st.clone())).await;
        Self {
            _dir: dir,
            st,
            url,
            c: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        }
    }
    async fn post(&self, path: &str, data: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", "tokomotive_session=secrets-owner")
            .form(data)
            .send()
            .await
            .unwrap()
    }
    async fn html(&self, path: &str) -> String {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", "tokomotive_session=secrets-owner")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }
    async fn store(&self, kind: &str, address: &str, credentials: &Value) {
        let credentials = credentials.to_string();
        let r = self
            .post(
                "/admin/settings/secrets",
                &[
                    ("name", "test"),
                    ("kind", kind),
                    ("address", address),
                    ("allow_private", "on"),
                    ("credentials", &credentials),
                ],
            )
            .await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    }
}

async fn check_store_binding_change(change: &str) {
    let live = serve(Router::new().route(
        "/v1/kv/data/llm",
        get(|headers: axum::http::HeaderMap| async move {
            if headers["x-vault-token"] == "fixture" {
                (
                    axum::http::StatusCode::OK,
                    Json(json!({"data":{"data":{"key":"bound-to-original-store"}}})),
                )
            } else {
                (axum::http::StatusCode::FORBIDDEN, Json(json!({})))
            }
        }),
    ))
    .await;
    let failing =
        serve(Router::new().fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }))
            .await;
    let a = App::new().await;
    assert_eq!(
        a.post(
            "/admin/settings/secrets",
            &[
                ("name", "test"),
                ("kind", "vault"),
                ("address", &live),
                ("credentials", r#"{"token":"fixture"}"#),
                ("allow_private", "on"),
                ("is_default", "on")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "p"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.st.router_table().providers[0].keys[0],
        "bound-to-original-store"
    );
    let response = match change {
        "delete" => a.post("/admin/settings/secrets/test/delete", &[]).await,
        "default" => {
            a.post(
                "/admin/settings/secrets",
                &[
                    ("name", "replacement"),
                    ("kind", "vault"),
                    ("address", &failing),
                    ("credentials", r#"{"token":"fixture"}"#),
                    ("allow_private", "on"),
                    ("is_default", "on"),
                ],
            )
            .await
        }
        "address" => {
            a.post(
                "/admin/settings/secrets",
                &[
                    ("name", "test"),
                    ("kind", "vault"),
                    ("address", &failing),
                    ("allow_private", "on"),
                    ("is_default", "on"),
                ],
            )
            .await
        }
        "auth" => {
            a.post(
                "/admin/settings/secrets",
                &[
                    ("name", "test"),
                    ("kind", "vault"),
                    ("address", &live),
                    ("credentials", r#"{"token":"changed"}"#),
                    ("allow_private", "on"),
                    ("is_default", "on"),
                ],
            )
            .await
        }
        "options" => {
            a.post(
                "/admin/settings/secrets",
                &[
                    ("name", "test"),
                    ("kind", "vault"),
                    ("address", &live),
                    ("options", r#"{"kv_version":"1"}"#),
                    ("allow_private", "on"),
                    ("is_default", "on"),
                ],
            )
            .await
        }
        _ => unreachable!(),
    };
    assert_eq!(response.status(), 303, "{change}");
    let user = db::create_user(&a.st.db, "binding-test", None, None, false)
        .await
        .unwrap();
    let (_, key) = db::create_key(&a.st.db, user, "test", None).await.unwrap();
    let response =
        a.c.post(format!("{}/v1/chat/completions", a.url))
            .bearer_auth(key)
            .json(&json!({"model":"p/test","messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), 503, "{change}");
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("secret unavailable"),
        "{change}"
    );
    assert!(
        a.st.secrets.get("p", "vault:kv/llm#key").is_none(),
        "{change}"
    );
}

#[tokio::test]
async fn store_binding_delete_discards_the_cached_value() {
    check_store_binding_change("delete").await;
}
#[tokio::test]
async fn store_binding_default_switch_discards_the_previous_store_value() {
    check_store_binding_change("default").await;
}
#[tokio::test]
async fn store_binding_address_change_discards_the_cached_value() {
    check_store_binding_change("address").await;
}
#[tokio::test]
async fn store_binding_auth_and_options_changes_discard_the_cached_value() {
    for change in ["auth", "options"] {
        check_store_binding_change(change).await;
    }
}

#[tokio::test]
async fn vault_reference_is_stored_not_its_value() {
    let secret = ["runtime", "only", "provider", "credential"].join("-");
    let payload = json!({"data":{"data":{"openai":secret}}});
    let model_secret = secret.clone();
    let vault = serve(
        Router::new()
            .route(
                "/v1/kv/data/llm",
                get(move || {
                    let p = payload.clone();
                    async move { Json(p) }
                }),
            )
            .route(
                "/v1/models",
                get(move |headers: axum::http::HeaderMap| {
                    let expected = model_secret.clone();
                    async move {
                        assert_eq!(
                            headers["authorization"].to_str().unwrap(),
                            format!("Bearer {expected}")
                        );
                        Json(json!({"data":[{"id":"from-secret-store"}]}))
                    }
                }),
            ),
    )
    .await;
    let a = App::new().await;
    a.store(
        "vault",
        &vault,
        &json!({"token":(["demo", "token"].join("-"))}),
    )
    .await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("name", "openai"),
                ("kind", "openai"),
                ("auth", "api_key"),
                ("base_url", &vault),
                ("api_key", "vault:kv/llm#openai"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    assert_eq!(
        a.st.router_table().providers[0].keys.as_ref(),
        std::slice::from_ref(&secret)
    );
    let raw: String = sqlx::query_scalar("SELECT keys_json FROM providers WHERE name='openai'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap(),
        json!(["vault:kv/llm#openai"])
    );
    assert!(!a.html("/admin/providers").await.contains(&secret));
    assert!(!format!("{:?}", a.st.router_table().providers[0]).contains(&secret));
    let hits: Value =
        a.c.get(format!(
            "{}/admin/search/models?provider=openai&src=provider&q=from-secret&limit=20",
            a.url
        ))
        .header("cookie", "tokomotive_session=secrets-owner")
        .header("accept", "application/json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == "from-secret-store")
    );
    let page = a.html("/admin/settings/secrets").await;
    assert!(page.contains("Secret stores") && page.contains("Edit store"));
    assert!(!page.contains(&secret));
    let providers = a.html("/admin/providers").await;
    assert!(
        providers.contains("data-secret-picker") && providers.contains("Advanced: raw reference")
    );
    let bytes = std::fs::read(a._dir.path().join("db.sqlite")).unwrap();
    assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
}

// Generate test-only tokens and ciphertext at runtime, not secret-shaped fixtures.
fn bws_key(seed: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut extract = Hmac::<Sha256>::new_from_slice(b"bitwarden-accesstoken").unwrap();
    extract.update(seed);
    let prk = extract.finalize().into_bytes();
    let mut out = Vec::new();
    let mut last = Vec::new();
    for n in 1..=2 {
        let mut h = Hmac::<Sha256>::new_from_slice(&prk).unwrap();
        h.update(&last);
        h.update(b"sm-access-token");
        h.update(&[n]);
        last = h.finalize().into_bytes().to_vec();
        out.extend_from_slice(&last);
    }
    out
}
fn enc(key: &[u8], plain: &[u8]) -> String {
    use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    use hmac::{Hmac, Mac};
    let iv = rand::random::<[u8; 16]>();
    let ct = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key[..32], &iv)
        .unwrap()
        .encrypt_padded_vec_mut::<Pkcs7>(plain);
    let mut h = Hmac::<sha2::Sha256>::new_from_slice(&key[32..]).unwrap();
    h.update(&iv);
    h.update(&ct);
    format!(
        "2.{}|{}|{}",
        B64.encode(iv),
        B64.encode(ct),
        B64.encode(h.finalize().into_bytes())
    )
}
#[tokio::test]
async fn bws_native_login_decrypts_payload_and_secret() {
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    let seed = rand::random::<[u8; 16]>();
    let org = rand::random::<[u8; 64]>();
    let secret = ["BWS", "in", "memory", "only"].join("-");
    let encrypted = enc(&org, secret.as_bytes());
    let payload = enc(
        &bws_key(&seed),
        json!({"encryptionKey":B64.encode(org)})
            .to_string()
            .as_bytes(),
    );
    let jwt = format!(
        "e30.{}.ignored",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"organization":"org"}"#)
    );
    let login = json!({"access_token":jwt,"encrypted_payload":payload,"expires_in":3600});
    let api = serve(
        Router::new()
            .route(
                "/identity/connect/token",
                axum::routing::post(move || {
                    let v = login.clone();
                    async move { Json(v) }
                }),
            )
            .route(
                "/api/secrets/123",
                get(move || {
                    let v = json!({"value":encrypted});
                    async move { Json(v) }
                }),
            ),
    )
    .await;
    let a = App::new().await;
    let token = format!(
        "0.{}.{}:{}",
        ["00000000", "0000", "0000", "0000", "000000000001"].join("-"),
        hex::encode(rand::random::<[u8; 16]>()),
        B64.encode(seed)
    );
    a.store("bws", &api, &json!({"access_token":token})).await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("name", "bws-test"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "bws:123"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(a.st.router_table().providers[0].keys.as_ref(), &[secret]);
}

#[tokio::test]
async fn rotation_reaches_proxy_and_failure_keeps_last_good_with_health_warning() {
    use std::sync::{Arc, RwLock};
    let value = Arc::new(RwLock::new(Some(
        "first-rotating-provider-value".to_owned(),
    )));
    let v = value.clone();
    let vault = serve(Router::new().route(
        "/v1/kv/data/llm",
        get(move || {
            let v = v.clone();
            async move {
                match v.read().unwrap().clone() {
                    Some(s) => (
                        axum::http::StatusCode::OK,
                        Json(json!({"data":{"data":{"key":s}}})),
                    ),
                    None => (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error":"do not render this payload"})),
                    ),
                }
            }
        }),
    ))
    .await;
    let seen = Arc::new(RwLock::new(Vec::new()));
    let capture = seen.clone();
    let upstream=serve(Router::new().route("/v1/chat/completions",axum::routing::post(move |headers:axum::http::HeaderMap| { let seen=capture.clone();async move {
        seen.write().unwrap().push(headers["authorization"].to_str().unwrap().to_owned());
        Json(json!({"id":"mock","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
    }}))).await;
    let a = App::new().await;
    a.store("vault", &vault, &json!({"token":"fixture"})).await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "up"),
                ("kind", "openai"),
                ("base_url", &upstream),
                ("api_key", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    let user = db::create_user(&a.st.db, "test", None, None, false)
        .await
        .unwrap();
    let (_, key) = db::create_key(&a.st.db, user, "test", None).await.unwrap();
    for expected in [
        "first-rotating-provider-value",
        "second-rotating-provider-value",
        "second-rotating-provider-value",
    ] {
        if seen.read().unwrap().len() == 1 {
            *value.write().unwrap() = Some(expected.into());
            a.st.refresh_secrets(true).await.unwrap();
        }
        if seen.read().unwrap().len() == 2 {
            *value.write().unwrap() = None;
            a.st.refresh_secrets(true).await.unwrap();
        }
        let r =
            a.c.post(format!("{}/v1/chat/completions", a.url))
                .bearer_auth(&key)
                .json(&json!({"model":"up/test","messages":[{"role":"user","content":"hello"}]}))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        assert_eq!(
            seen.read().unwrap().last().unwrap(),
            &format!("Bearer {expected}")
        );
    }
    let html = a.html("/admin/providers").await;
    assert!(html.contains("secret unavailable"));
    assert!(!html.contains("second-rotating-provider-value"));
    assert!(!html.contains("do not render this payload"));
    a.st.flush().await;
    let audit = a.html("/admin/audit").await;
    assert!(!audit.contains("first-rotating-provider-value"));
    for entry in std::fs::read_dir(a._dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(path).unwrap();
        for value in [
            "first-rotating-provider-value",
            "second-rotating-provider-value",
        ] {
            assert!(!bytes.windows(value.len()).any(|w| w == value.as_bytes()));
        }
    }
}

#[tokio::test]
async fn secret_picker_paginates_and_test_only_returns_masked_value() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = calls.clone();
    let api = serve(Router::new().route(
        "/",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let seen = seen.clone();
                async move {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    let authorization = headers["authorization"].to_str().unwrap();
                    assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=fixture/"));
                    assert!(
                        authorization
                            .contains("/eu-west-1/secretsmanager/aws4_request, SignedHeaders=")
                    );
                    assert_eq!(authorization.rsplit("Signature=").next().unwrap().len(), 64);
                    seen.lock().unwrap().push(body.clone());
                    Json(if body.get("SecretId").is_some() {
                        json!({"SecretString":"masked-secret-value-tail"})
                    } else if body.get("NextToken").is_some() {
                        json!({"SecretList":[{"Name":"second","ARN":"second"}]})
                    } else {
                        json!({"SecretList":[{"Name":"first","ARN":"first"}],"NextToken":"page-2"})
                    })
                }
            },
        ),
    ))
    .await;
    let a = App::new().await;
    a.store(
        "aws-sm",
        &api,
        &json!({"access_key":"fixture","secret_key":"fixture"}),
    )
    .await;
    sqlx::query("UPDATE secret_stores SET options=?")
        .bind(json!({"region":"eu-west-1"}).to_string())
        .execute(&a.st.db)
        .await
        .unwrap();
    let first: Value =
        a.c.get(format!("{}/admin/secrets/browse?store=test", a.url))
            .header("cookie", "tokomotive_session=secrets-owner")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(first["items"][0]["label"], "first");
    assert_eq!(first["next"], "page-2");
    let next: Value =
        a.c.get(format!(
            "{}/admin/secrets/browse?store=test&cursor=page-2",
            a.url
        ))
        .header("cookie", "tokomotive_session=secrets-owner")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(next["items"][0]["label"], "second");
    assert!(next["next"].is_null());
    assert_eq!(calls.lock().unwrap()[1]["NextToken"], "page-2");
    let r = a
        .post("/admin/secrets/test", &[("reference", "aws-sm:first")])
        .await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("****tail"));
    assert!(!text.contains("masked-secret-value-tail"));
}

#[tokio::test]
async fn vault_approle_namespace_and_kv1_are_supported() {
    let api = serve(
        Router::new()
            .route(
                "/v1/auth/approle/login",
                axum::routing::post(
                    |headers: axum::http::HeaderMap, Json(v): Json<Value>| async move {
                        assert_eq!(headers["x-vault-namespace"], "team");
                        assert_eq!(v["role_id"], "role");
                        assert_eq!(v["secret_id"], "bootstrap");
                        Json(json!({"auth":{"client_token":"session","lease_duration":1}}))
                    },
                ),
            )
            .route(
                "/v1/kv/llm",
                get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["x-vault-token"], "session");
                    assert_eq!(headers["x-vault-namespace"], "team");
                    Json(json!({"data":{"key":"kv-one-value"}}))
                }),
            ),
    )
    .await;
    let a = App::new().await;
    a.store(
        "vault",
        &api,
        &json!({"role_id":"role","secret_id":"bootstrap"}),
    )
    .await;
    sqlx::query("UPDATE secret_stores SET options=?")
        .bind(json!({"kv_version":"1","namespace":"team"}).to_string())
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "v1"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(a.st.router_table().providers[0].keys[0], "kv-one-value");
    a.st.refresh_secrets(true).await.unwrap();
    assert!(a.st.secrets.unavailable("v1").is_empty());
}

#[tokio::test]
async fn gcp_rs256_exchange_then_access_and_field_selection() {
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    use rsa::pkcs8::EncodePrivateKey;
    let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let pem = private.to_pkcs8_pem(Default::default()).unwrap();
    let public = rsa::pkcs8::EncodePublicKey::to_public_key_pem(
        &private.to_public_key(),
        Default::default(),
    )
    .unwrap();
    let api = serve(
        Router::new()
            .route(
                "/token",
                axum::routing::post(
                    move |axum::Form(f): axum::Form<std::collections::HashMap<String, String>>| {
                        let public = public.clone();
                        async move {
                            assert_eq!(
                                f["grant_type"],
                                "urn:ietf:params:oauth:grant-type:jwt-bearer"
                            );
                            let mut validation =
                                jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
                            validation.validate_aud = false;
                            let claims = jsonwebtoken::decode::<Value>(
                                &f["assertion"],
                                &jsonwebtoken::DecodingKey::from_rsa_pem(public.as_bytes())
                                    .unwrap(),
                                &validation,
                            )
                            .unwrap();
                            assert_eq!(claims.claims["iss"], "test@example.invalid");
                            Json(json!({"access_token":"gcp-session","expires_in":3600}))
                        }
                    },
                ),
            )
            .route(
                "/v1/projects/p/secrets/s/versions/latest:access",
                get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer gcp-session");
                    Json(
                        json!({"payload":{"data":B64.encode(br#"{"key":"google-secret-value"}"#)}}),
                    )
                }),
            ),
    )
    .await;
    let a = App::new().await;
    a.store("gcp-sm",&api,&json!({"service_account":json!({"client_email":"test@example.invalid","private_key":pem.as_str()}).to_string()})).await;
    sqlx::query("UPDATE secret_stores SET options=?")
        .bind(json!({"token_url":format!("{api}/token"),"project":"p"}).to_string())
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "gcp"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "gcp-sm:projects/p/secrets/s#key")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.st.router_table().providers[0].keys[0],
        "google-secret-value"
    );
}

#[tokio::test]
async fn bw_serve_custom_fields_and_locked_vault_are_clear() {
    let locked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let l = locked.clone();
    let api=serve(Router::new().route("/status",get(move || {let l=l.clone();async move {Json(json!({"data":{"template":{"status":if l.load(std::sync::atomic::Ordering::Relaxed){"locked"}else{"unlocked"}}}}))}}))
        .route("/object/item/item",get(||async {Json(json!({"data":{"login":{"password":"password-value","username":"user","uris":[{"uri":"https://example.invalid"}]},"notes":"notes","fields":[{"name":"openai","value":"password-manager-key"}]}}))}))).await;
    let a = App::new().await;
    a.store("bw", &api, &json!({})).await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "bw"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "bw:item#openai")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.st.router_table().providers[0].keys[0],
        "password-manager-key"
    );
    locked.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = a
        .post("/admin/secrets/test", &[("reference", "bw:item#openai")])
        .await;
    assert!(!r.status().is_success());
    assert!(r.text().await.unwrap().contains("vault locked"));
    a.st.refresh_secrets(true).await.unwrap();
    assert_eq!(
        a.st.router_table().providers[0].keys[0],
        "password-manager-key"
    );
}

#[tokio::test]
async fn owner_only_references_and_public_http_denied() {
    let a = App::new().await;
    let r = a
        .post(
            "/admin/settings/secrets",
            &[
                ("name", "public"),
                ("kind", "bw"),
                ("address", "http://8.8.8.8"),
                ("allow_private", "on"),
                ("credentials", "{}"),
            ],
        )
        .await;
    assert!(!r.status().is_success());
    assert!(r.text().await.unwrap().contains("store address denied"));
    sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) VALUES('nonowner','fixture','admin')").execute(&a.st.db).await.unwrap();
    let account = tokomotive::admin::by_username(&a.st.db, "nonowner")
        .await
        .unwrap()
        .unwrap();
    tokomotive::admin::create_session(&a.st.db, "admin-session", 1, &account)
        .await
        .unwrap();
    let response =
        a.c.post(format!("{}/admin/providers", a.url))
            .header("cookie", "tokomotive_session=admin-session")
            .form(&[
                ("name", "bad"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "vault:kv/path#key"),
            ])
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), 403);
    for path in [
        "/admin/settings/secrets",
        "/admin/secrets/stores",
        "/admin/secrets/browse?store=test",
    ] {
        assert_eq!(
            a.c.get(format!("{}{path}", a.url))
                .header("cookie", "tokomotive_session=admin-session")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
}

#[tokio::test]
async fn never_resolved_reference_fails_closed_without_silent_provider_fallback() {
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = requests.clone();
    let upstream = serve(Router::new().fallback(move || {
        let n = count.clone();
        async move {
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Json(json!({"ok":true}))
        }
    }))
    .await;
    let a = App::new().await;
    // A nonexistent store is valid configuration, but its key is unusable.
    for (name, key) in [
        ("broken", "vault:kv/missing#key"),
        ("working", "fixture-key"),
    ] {
        assert_eq!(
            a.post(
                "/admin/providers",
                &[
                    ("name", name),
                    ("kind", "openai"),
                    ("base_url", &upstream),
                    ("api_key", key)
                ]
            )
            .await
            .status(),
            303
        );
    }
    assert_eq!(
        a.post(
            "/admin/routes",
            &[
                ("name", "safe"),
                ("strategy", "priority"),
                ("deployment_provider", "broken"),
                ("deployment_model", "test"),
                ("deployment_weight", "1"),
                ("deployment_provider", "working"),
                ("deployment_model", "test"),
                ("deployment_weight", "1")
            ]
        )
        .await
        .status(),
        303
    );
    let user = db::create_user(&a.st.db, "test", None, None, false)
        .await
        .unwrap();
    let (_, key) = db::create_key(&a.st.db, user, "test", None).await.unwrap();
    let r =
        a.c.post(format!("{}/v1/chat/completions", a.url))
            .bearer_auth(key)
            .json(&json!({"model":"safe","messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 503);
    assert!(r.text().await.unwrap().contains("secret unavailable"));
    assert_eq!(requests.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn redirects_and_large_responses_do_not_replace_cached_secrets() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mode = Arc::new(AtomicUsize::new(0));
    let m = mode.clone();
    let followed = Arc::new(AtomicUsize::new(0));
    let f = followed.clone();
    let other = serve(Router::new().fallback(move || {
        let f = f.clone();
        async move {
            f.fetch_add(1, Ordering::Relaxed);
            "not a secret"
        }
    }))
    .await;
    let api = serve(Router::new().route(
        "/v1/kv/data/llm",
        get(move || {
            let m = m.clone();
            let other = other.clone();
            async move {
                use axum::response::IntoResponse;
                match m.load(Ordering::Relaxed) {
                    0 => Json(json!({"data":{"data":{"key":"last-good-provider-key"}}}))
                        .into_response(),
                    1 => (axum::http::StatusCode::FOUND, [("location", other)]).into_response(),
                    _ => "x".repeat(1024 * 1024 + 1).into_response(),
                }
            }
        }),
    ))
    .await;
    let a = App::new().await;
    a.store("vault", &api, &json!({"token":"fixture"})).await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "safe"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    for n in [1, 2] {
        mode.store(n, Ordering::Relaxed);
        a.st.refresh_secrets(true).await.unwrap();
        assert_eq!(
            a.st.router_table().providers[0].keys[0],
            "last-good-provider-key"
        );
        assert!(!a.st.secrets.unavailable("safe").is_empty());
    }
    assert_eq!(followed.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn resolution_uses_provider_egress_proxy() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = calls.clone();
    let proxy = serve(
        Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let n = n.clone();
            async move {
                assert_eq!(headers["x-vault-token"], "bootstrap");
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Json(json!({"data":{"data":{"key":"via-the-provider-egress-proxy"}}}))
            }
        }),
    )
    .await;
    let a = App::new().await;
    a.store("vault", "http://127.0.0.1:9", &json!({"token":"bootstrap"}))
        .await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "p"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/providers/p/egress",
            &[("mode", "custom"), ("proxy_url", &proxy)]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.st.router_table().providers[0].keys[0],
        "via-the-provider-egress-proxy"
    );
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn startup_and_store_interval_resolve_again_without_persisting_values() {
    let value = std::sync::Arc::new(std::sync::RwLock::new("before-restart".to_owned()));
    let v = value.clone();
    let api = serve(Router::new().route(
        "/v1/kv/data/llm",
        get(move |headers: axum::http::HeaderMap| {
            let v = v.clone();
            async move {
                assert_eq!(
                    headers["x-vault-token"].to_str().unwrap(),
                    std::env::var("HOME").unwrap()
                );
                Json(json!({"data":{"data":{"key":v.read().unwrap().clone()}}}))
            }
        }),
    ))
    .await;
    let a = App::new().await;
    a.store("vault", &api, &json!({"token":"env:HOME"})).await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "p"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("secret_reference", "vault:kv/llm#key")
            ]
        )
        .await
        .status(),
        303
    );
    *value.write().unwrap() = "after-restart".into();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", a._dir.path().display());
    let restarted = tokomotive::init(&cfg, None).await.unwrap();
    assert_eq!(
        restarted.router_table().providers[0].keys[0],
        "after-restart"
    );
    sqlx::query("UPDATE secret_stores SET refresh_secs=1")
        .execute(&restarted.db)
        .await
        .unwrap();
    *value.write().unwrap() = "timer-rotation".into();
    let task = tokomotive::secrets::start_background(restarted.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if restarted.router_table().providers[0].keys[0] == "timer-rotation" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let stored: String = sqlx::query_scalar("SELECT credentials FROM secret_stores")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert!(!stored.contains("env:HOME"));
    assert!(!stored.contains(&std::env::var("HOME").unwrap()));
}
