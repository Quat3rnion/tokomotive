use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokomotive::{config::Config, db};
#[path = "support/oauth.rs"]
mod mock_oauth;

#[tokio::test]
async fn attempts_expire_and_reject_malformed_pasted_urls_before_exchange() {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('chat','\"openai\"','https://api.openai.com','chatgpt')").execute(&db).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let auth = OAuthManager::new(
        db,
        SecretKey::from_bytes([9; 32]),
        OAuthEndpoints::default(),
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    for bad in [
        "?state={state}",
        "?code=c&state=wrong",
        "?code=a&code=b&state={state}",
        "?code=c&state={state}#fragment",
        "?error=denied&state={state}",
    ] {
        let a = auth.start("chat", Flow::Codex).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert!(
            auth.finish(
                "chat",
                Flow::Codex,
                &format!(
                    "http://localhost:1455/auth/callback{}",
                    bad.replace("{state}", &q["state"])
                )
            )
            .await
            .is_err()
        );
    }
    let a = auth.start("chat", Flow::Codex).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    clock.store(a.expires_at, Ordering::SeqCst);
    assert!(
        auth.finish(
            "chat",
            Flow::Codex,
            &format!(
                "http://localhost:1455/auth/callback?code=c&state={}",
                q["state"]
            )
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("expired")
    );
}

#[tokio::test]
async fn refresh_failure_disconnects_and_cannot_replay_after_reload() {
    use axum::{http::StatusCode, response::IntoResponse};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/token",post(move || {let seen=seen.clone();async move {
        if seen.fetch_add(1,Ordering::SeqCst)==0 {Json(json!({"access_token":"access","refresh_token":"refresh","expires_in":1})).into_response()}
        else {(StatusCode::INTERNAL_SERVER_ERROR,"sensitive upstream body").into_response()}
    }}))).await.unwrap()
    });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('sub','\"anthropic\"','https://api.anthropic.com','claude')").execute(&db).await.unwrap();
    let endpoints = OAuthEndpoints {
        claude_token: format!("{base}/token"),
        claude_resource: base,
        ..Default::default()
    };
    let key = SecretKey::from_bytes([2; 32]);
    let auth = Arc::new(OAuthManager::new(db.clone(), key.clone(), endpoints.clone()).unwrap());
    let a = auth.start("sub", Flow::Claude).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    auth.finish("sub", Flow::Claude, &format!("c#{}", q["state"]))
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let auth = auth.clone();
        tasks.spawn(async move {
            assert!(auth.token("sub", Flow::Claude, None).await.is_err());
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    let reloaded = OAuthManager::new(db, key, endpoints).unwrap();
    assert!(reloaded.token("sub", Flow::Claude, None).await.is_err());
    let status = reloaded.status("sub", Flow::Claude).await.unwrap();
    assert!(!status.connected);
    assert_eq!(
        status.last_error.as_deref(),
        Some("Refresh did not complete; sign in again")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    mock.abort();
}

#[tokio::test]
async fn disconnected_subscription_counts_as_circuit_failure_and_falls_back() {
    use tokomotive::provider::Endpoint;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/v1/chat/completions",post(|| async {Json(json!({"model":"m","choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1}}))}))).await.unwrap()
    });
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database='sqlite::memory:'
[router]
failure_threshold=1
[[providers]]
name='sub'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
[[providers]]
name='paid'
kind='openai'
base_url='{base}'
[[routes]]
name='smart'
deployments=[{{provider='sub',model='m'}},{{provider='paid',model='m'}}]
[[users]]
name='user'
keys=['vk-fallback']
key_allowed_providers=['sub','paid']
"#
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("vk-fallback")
        .json(&json!({"model":"smart","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    st.flush().await;
    let table = st.router_table();
    let candidates = st.candidates(
        &table,
        &st.catalog(),
        Some("smart"),
        Endpoint::ChatCompletions,
        false,
    );
    assert!(!st.deployment_available(&candidates[0], &table.settings));
    let row: (String, i64) = sqlx::query_as("SELECT provider,attempts FROM ledger")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(row, ("paid".into(), 2));
    server.abort();
    mock.abort();
}

#[tokio::test]
async fn chatgpt_verifies_registration_and_reuses_the_user_owned_client() {
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    let mock = mock_oauth::MockOAuth::start().await;
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='chat'\nkind='openai'\nauth='chatgpt'\nbase_url='https://api.openai.com'\n[[users]]\nname='user'\nkeys=['vk-chat']\nkey_allowed_providers=['chat']").unwrap();
    let db = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            issuer: mock.base.clone(),
            openai_resource: format!("{}/v1", mock.base),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let auth = &st.oauth;
    for forged in [true, false] {
        let a = auth.start("chat", Flow::Chatgpt).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["client_id"], "dynamic_agent_client");
        assert_eq!(q["resource"], format!("{}/v1", mock.base));
        assert_eq!(q["ext_agent_host_id"].len(), 45);
        mock.claims.lock().unwrap()["nonce"] = json!(q["nonce"]);
        *mock.forged.lock().unwrap() = forged;
        let result=auth.finish("chat",Flow::Chatgpt,&format!("http://localhost:1455/auth/callback?state={}&code=code&client_id=issued-client",q["state"])).await;
        assert_eq!(result.is_err(), forged);
    }
    let status = auth.status("chat", Flow::Chatgpt).await.unwrap();
    assert!(status.connected);
    assert_eq!(status.account.as_deref(), Some("op@example.test"));
    assert_eq!(status.plan.as_deref(), Some("plus"));
    assert_eq!(
        auth.token("chat", Flow::Chatgpt, None).await.unwrap(),
        "chatgpt-access"
    );
    assert!(
        auth.start("chat", Flow::Chatgpt)
            .await
            .unwrap()
            .authorize_url
            .contains("client_id=issued-client")
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 2);
    for (field, value) in [
        ("nonce", json!("wrong")),
        ("iss", json!("https://wrong.example")),
        ("aud", json!("wrong-client")),
        ("exp", json!(0)),
    ] {
        let a = auth.start("chat", Flow::Chatgpt).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let previous = mock.claims.lock().unwrap().clone();
        mock.claims.lock().unwrap()["nonce"] = json!(q["nonce"]);
        mock.claims.lock().unwrap()[field] = value;
        assert!(
            auth.finish(
                "chat",
                Flow::Chatgpt,
                &format!(
                    "http://localhost:1455/auth/callback?code=c&state={}",
                    q["state"]
                )
            )
            .await
            .is_err()
        );
        *mock.claims.lock().unwrap() = previous;
    }
    let a = auth.start("chat", Flow::Chatgpt).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    *mock.scope.lock().unwrap() = "openid".into();
    assert!(
        auth.finish(
            "chat",
            Flow::Chatgpt,
            &format!(
                "http://localhost:1455/auth/callback?code=c&state={}",
                q["state"]
            )
        )
        .await
        .is_err()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .bearer_auth("vk-chat")
        .json(&json!({"model":"gpt-test","input":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    st.flush().await;
    let row: (String, i64, i64, i64) =
        sqlx::query_as("SELECT provider,input,output,cost_micros FROM ledger")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(row, ("chat".into(), 7, 2, 0));
    server.abort();
}

#[tokio::test]
async fn codex_login_reads_usage_only_and_unavailable_is_optional() {
    use axum::http::HeaderMap;
    use axum::routing::get;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let id=format!("h.{}.s",URL_SAFE_NO_PAD.encode(json!({"email":"op@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"acct","chatgpt_plan_type":"pro"}}).to_string()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new()
        .route("/oauth/token",post(move || {let id=id.clone();async move {Json(json!({"access_token":"codex-access","refresh_token":"codex-refresh","id_token":id,"expires_in":3600}))}}))
        .route("/usage",get(|headers:HeaderMap| async move {
            assert_eq!(headers["chatgpt-account-id"],"acct");assert_eq!(headers["originator"],"codex_cli_rs");assert_eq!(headers["authorization"],"Bearer codex-access");
            Json(json!({"rate_limit":{"primary_window":{"used_percent":42,"limit_window_seconds":18000,"reset_after_seconds":60},"secondary_window":{"used_percent":84,"limit_window_seconds":604800,"reset_at":1794618000}}}))
        }))).await.unwrap()
    });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('chat','\"openai\"','https://api.openai.com','chatgpt')").execute(&db).await.unwrap();
    let auth = OAuthManager::new(
        db,
        SecretKey::from_bytes([3; 32]),
        OAuthEndpoints {
            issuer: base.clone(),
            codex_usage: format!("{base}/usage"),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(auth.usage("chat").await.is_none());
    let a = auth.start("chat", Flow::Codex).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(q["originator"], "codex_cli_rs");
    assert_eq!(q["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    auth.finish(
        "chat",
        Flow::Codex,
        &format!(
            "http://localhost:1455/auth/callback?code=c&state={}",
            q["state"]
        ),
    )
    .await
    .unwrap();
    let usage = auth.usage("chat").await.unwrap();
    assert_eq!(usage.windows.len(), 2);
    assert_eq!(usage.windows[0].label, "5-hour");
    assert_eq!(usage.windows[0].used_percent, 42.0);
    assert_eq!(usage.windows[1].label, "Weekly");
    assert_eq!(usage.windows[1].resets_at, Some(1794618000000));
    assert!(auth.token("chat", Flow::Chatgpt, None).await.is_err());
    auth.disconnect("chat", Flow::Codex).await.unwrap();
    assert!(auth.usage("chat").await.is_none());
    mock.abort();
}

#[tokio::test]
async fn dashboard_creates_subscription_and_edits_provider_access_without_secrets() {
    use tokomotive::{oauth::OAuthEndpoints, state::AppState};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/token",post(|| async {Json(json!({"access_token":"html-access-secret","refresh_token":"html-refresh-secret","expires_in":3600}))}))).await.unwrap()
    });
    let cfg: Config = toml::from_str("[server]\ndatabase='sqlite::memory:'").unwrap();
    let db = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            claude_token: format!("{base}/token"),
            claude_resource: base,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let user = db::create_user(&st.db, "user", None, None, false)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    c.post(format!("{url}/setup"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    let r = c
        .post(format!("{url}/admin/login"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let r = c
        .post(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "subscription"),
            ("kind", "anthropic"),
            ("auth", "claude"),
            ("base_url", "https://api.anthropic.com"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(st.router_table().providers[0].auth.as_str(), "claude");
    let r = c
        .post(format!("{url}/admin/keys"))
        .header("cookie", &cookie)
        .form(&[
            ("user_id", user.to_string()),
            ("allowed_providers", "subscription".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("name=\"allowed_providers\""));
    let keys = db::list_keys(&st.db).await.unwrap();
    let id = keys[0].id;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT allowed_providers FROM virtual_keys WHERE id=?")
            .bind(id)
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("[\"subscription\"]"));
    let r = c
        .post(format!("{url}/admin/keys/{id}/providers"))
        .header("cookie", &cookie)
        .form(&Vec::<(String, String)>::new())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let r = c
        .post(format!("{url}/admin/providers/subscription/oauth/start"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("target=\"_blank\""));
    assert!(html.contains("code_challenge"));
    assert!(!html.contains("code_verifier"));
    let href = html
        .split("href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&amp;", "&")
        .replace("&#38;", "&");
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&href)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    let r = c
        .post(format!("{url}/admin/providers/subscription/oauth/finish"))
        .header("cookie", &cookie)
        .form(&[("callback", format!("test-code#{}", q["state"]))])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let html = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Connected"));
    assert!(!html.contains("html-access-secret"));
    assert!(!html.contains("html-refresh-secret"));
    assert!(!html.contains("test-code"));
    let r = c
        .post(format!("{url}/admin/providers/subscription"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "subscription"),
            ("kind", "anthropic"),
            ("auth", "claude"),
            ("base_url", "https://api.anthropic.com"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert!(
        st.oauth
            .status("subscription", tokomotive::oauth::Flow::Claude)
            .await
            .unwrap()
            .connected
    );
    let r = c
        .post(format!("{url}/admin/providers/subscription/keys"))
        .header("cookie", &cookie)
        .form(&[("api_key", "must-not-be-saved")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    st.reload_settings().await.unwrap();
    let r = c
        .post(format!(
            "{url}/admin/providers/subscription/oauth/disconnect"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert!(
        !st.oauth
            .status("subscription", tokomotive::oauth::Flow::Claude)
            .await
            .unwrap()
            .connected
    );
    server.abort();
    mock.abort();
}

#[tokio::test]
async fn oauth_attempt_validates_state_and_is_consumed() {
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('claude','\"anthropic\"','https://api.anthropic.com','claude')").execute(&db).await.unwrap();
    let auth = OAuthManager::new(
        db,
        SecretKey::from_bytes([7; 32]),
        OAuthEndpoints::default(),
    )
    .unwrap();
    let attempt = auth.start("claude", Flow::Claude).await.unwrap();
    let url = reqwest::Url::parse(&attempt.authorize_url).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"].len(), 43);
    assert_eq!(q["scope"], tokomotive::oauth::CLAUDE_SCOPES);
    assert_eq!(q["client_id"], tokomotive::oauth::CLAUDE_CLIENT);
    assert!(
        auth.finish("claude", Flow::Claude, "code#wrong-state")
            .await
            .unwrap_err()
            .to_string()
            .contains("state")
    );
    assert!(
        auth.finish("claude", Flow::Claude, &format!("code#{}", q["state"]))
            .await
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
    assert!(!auth.status("claude", Flow::Claude).await.unwrap().connected);
}

#[tokio::test]
async fn claude_exchange_encrypts_tokens_and_refresh_is_single_flight() {
    use std::sync::{Arc, Mutex};
    use tokomotive::{
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/token",post(move |headers:axum::http::HeaderMap,Json(body):Json<Value>| {
        let seen=seen.clone(); async move {
            assert_eq!(headers["content-type"],"application/json");
            assert_eq!(headers["user-agent"],"axios/1.13.6");
            assert_eq!(headers["accept"],"application/json, text/plain, */*");
            let mut seen=seen.lock().unwrap(); seen.push(body);
            Json(if seen.len()==1 { json!({"access_token":"secret-access-1","refresh_token":"secret-refresh-1","expires_in":1}) }
                else { json!({"access_token":format!("secret-access-{}",seen.len()),"refresh_token":format!("secret-refresh-{}",seen.len()),"expires_in":3600}) })
        }
    }))).await.unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let db = db::connect(&format!("sqlite://{}/db", dir.path().display()))
        .await
        .unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('claude','\"anthropic\"','https://api.anthropic.com','claude')").execute(&db).await.unwrap();
    let key = SecretKey::from_bytes([7; 32]);
    let endpoints = OAuthEndpoints {
        claude_token: format!("{base}/token"),
        claude_resource: base,
        ..Default::default()
    };
    let auth = Arc::new(OAuthManager::new(db.clone(), key.clone(), endpoints.clone()).unwrap());
    let a = auth.start("claude", Flow::Claude).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    auth.finish("claude", Flow::Claude, &format!("code#{}", q["state"]))
        .await
        .unwrap();
    use base64::Engine;
    use sha2::Digest;
    assert_eq!(
        q["code_challenge"],
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(
            requests.lock().unwrap()[0]["code_verifier"]
                .as_str()
                .unwrap()
        ))
    );
    let mut tasks = tokio::task::JoinSet::new();
    tokomotive::quota::record(
        &db,
        "claude",
        "signin",
        401,
        &axum::http::HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    for _ in 0..16 {
        let auth = auth.clone();
        tasks.spawn(async move { auth.token("claude", Flow::Claude, None).await.unwrap() });
    }
    while let Some(token) = tasks.join_next().await {
        assert_eq!(token.unwrap(), "secret-access-2");
    }
    assert_eq!(
        tokomotive::quota::accounts(&db, Some("claude"), 1)
            .await
            .unwrap()[0]
            .state,
        "ok",
        "a successful refresh re-enables inference"
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert_eq!(
        requests.lock().unwrap()[1]["refresh_token"],
        "secret-refresh-1"
    );
    let reloaded = Arc::new(OAuthManager::new(db.clone(), key, endpoints).unwrap());
    assert_eq!(
        reloaded.token("claude", Flow::Claude, None).await.unwrap(),
        "secret-access-2"
    );
    assert!(
        reloaded
            .status("claude", Flow::Claude)
            .await
            .unwrap()
            .last_refresh
            .is_some()
    );
    let mut retries = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let auth = reloaded.clone();
        retries.spawn(async move {
            auth.token("claude", Flow::Claude, Some("secret-access-2"))
                .await
                .unwrap()
        });
    }
    while let Some(token) = retries.join_next().await {
        assert_eq!(token.unwrap(), "secret-access-3");
    }
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert_eq!(
        requests.lock().unwrap()[2]["refresh_token"],
        "secret-refresh-2"
    );
    let stored: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!stored.contains("secret-access"));
    for path in [dir.path().join("db"), dir.path().join("db-wal")] {
        if let Ok(bytes) = std::fs::read(path) {
            assert!(!String::from_utf8_lossy(&bytes).contains("secret-refresh"));
        }
    }
    mock.abort();
}

#[tokio::test]
async fn translated_claude_subscription_retries_401_with_headers_and_zero_cost() {
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
    };
    use std::sync::{Arc, Mutex};
    use tokomotive::{
        oauth::{CLAUDE_BETAS, CLAUDE_IDENTITY, CLAUDE_UA, Flow, OAuthEndpoints},
        state::AppState,
    };
    let tokens = Arc::new(Mutex::new(0));
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let t = tokens.clone();
    let s = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener,Router::new()
        .route("/token",post(move |Json(body):Json<Value>| {let t=t.clone(); async move {
            let mut n=t.lock().unwrap(); *n+=1;
            if *n>1 { assert_eq!(body["refresh_token"],"refresh-1"); }
            Json(json!({"access_token":format!("access-{n}"),"refresh_token":format!("refresh-{n}"),"expires_in":3600}))
        }}))
        .route("/v1/messages",post(move |headers:HeaderMap,Json(body):Json<Value>| {let s=s.clone(); async move {
            assert!(!headers.contains_key("x-api-key"));
            assert_eq!(headers["anthropic-beta"],CLAUDE_BETAS); assert_eq!(headers["user-agent"],CLAUDE_UA);
            assert_eq!(headers["anthropic-version"],"2023-06-01");
            assert_eq!(body["system"][1]["text"],CLAUDE_IDENTITY);
            assert_eq!(body["system"][2]["text"],"original system");
            assert!(body["system"][0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header: cc_version=2.1.280."));
            assert_eq!(body["system"][0]["text"],"x-anthropic-billing-header: cc_version=2.1.280.21c; cc_entrypoint=sdk-cli; cch=56b10;");
            s.lock().unwrap().push(body);
            if headers["authorization"]=="Bearer access-1" { return StatusCode::UNAUTHORIZED.into_response(); }
            let model=s.lock().unwrap().last().unwrap()["model"].clone();
            Json(json!({"id":"msg-1","model":model,"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":3}})).into_response()
        }}))).await.unwrap()
    });
    let cfg: Config = toml::from_str(
        r#"
[server]
database='sqlite::memory:'
[[providers]]
name='sub'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
[[models]]
name='m'
provider='sub'
cost={input=2,output=4}
[[users]]
name='user'
keys=['vk-test']
key_allowed_providers=['sub']
"#,
    )
    .unwrap();
    let db = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            claude_token: format!("{base}/token"),
            claude_resource: base,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a = st.oauth.start("sub", Flow::Claude).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    st.oauth
        .finish("sub", Flow::Claude, &format!("code#{}", q["state"]))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let malformed = reqwest::Client::new()
        .post(format!("{url}/v1/messages"))
        .bearer_auth("vk-test")
        .json(&json!([]))
        .send()
        .await;
    assert_eq!(malformed.unwrap().status(), 400);
    let r=reqwest::Client::new().post(format!("{url}/v1/chat/completions")).bearer_auth("vk-test")
        .header("anthropic-beta","evil").header("user-agent","client-ua")
        .json(&json!({"model":"m","messages":[{"role":"system","content":"original system"},{"role":"user","content":"abcdHello world, this is a test"}]})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "done"
    );
    st.flush().await;
    assert_eq!(*tokens.lock().unwrap(), 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
    let row: (i64, i64, i64, i64, String) =
        sqlx::query_as("SELECT input,output,cost_micros,price_micros,provider FROM ledger")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(row, (10, 3, 0, 32, "sub".into()));
    let r=reqwest::Client::new().post(format!("{url}/v1/chat/completions")).bearer_auth("vk-test")
        .json(&json!({"model":"unknown","messages":[{"role":"system","content":"original system"},{"role":"user","content":"abcdHello world, this is a test"}]})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    st.flush().await;
    let id = db::create_model(&st.db, "unknown", "sub", &[])
        .await
        .unwrap();
    db::add_cost_card(
        &st.db,
        id,
        &tokomotive::pricing::CardRates {
            batch_multiplier_bp: None,
            base: tokomotive::pricing::Rates {
                input: 2_000_000,
                output: 4_000_000,
                ..Default::default()
            },
            tiers: vec![],
        },
    )
    .await
    .unwrap();
    st.reload_catalog().await.unwrap();
    assert_eq!(
        db::reprice_unpriced(&st.db, &st.catalog()).await.unwrap(),
        1
    );
    let row: (i64, i64) =
        sqlx::query_as("SELECT cost_micros,price_micros FROM ledger WHERE model='unknown'")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(row, (0, 32));
    server.abort();
    mock.abort();
}

#[tokio::test]
async fn subscription_provider_requires_explicit_key_access() {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", upstream.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(upstream, Router::new().route("/v1/chat/completions", post(|| async {
            Json(json!({"model":"m","choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1}}))
        }))).await.unwrap();
    });
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database = 'sqlite::memory:'
[[providers]]
name = 'subscription'
kind = 'openai'
auth = 'chatgpt'
base_url = 'https://api.openai.com'
[[providers]]
name = 'paid'
kind = 'openai'
base_url = '{base}'
[[routes]]
name = 'fallback'
deployments = [{{provider='subscription',model='m'}},{{provider='paid',model='m'}}]
[[routes]]
name = 'private'
deployments = [{{provider='subscription',model='m'}}]
[[users]]
name = 'user'
keys = ['vk-test']
"#
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::new();
    for model in ["private", "subscription/m"] {
        let r = c
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("vk-test")
            .json(&json!({"model":model,"messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert_eq!(
            r.json::<Value>().await.unwrap()["error"]["message"],
            "this API key may not use provider subscription"
        );
    }
    let r = c
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("vk-test")
        .json(&json!({"model":"fallback","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let models: Value = c
        .get(format!("{url}/v1/models"))
        .bearer_auth("vk-test")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "fallback");
    let key = db::lookup_key(&st.db, "vk-test").await.unwrap().unwrap();
    assert!(!key.revoked);
    server.abort();
    mock.abort();
}
