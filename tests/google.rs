use serde_json::json;
use tokomotive::{config::Config, provider::ProviderAuth};

#[tokio::test]
#[ignore = "Playwright fixture; set TOKOMOTIVE_PLAYWRIGHT to a local script"]
async fn google_forms_in_browser() {
    use tokomotive::{db, oauth::OAuthEndpoints, state::AppState};
    let script = std::env::var("TOKOMOTIVE_PLAYWRIGHT").expect("Set TOKOMOTIVE_PLAYWRIGHT");
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='cli'\nkind='gemini'\nauth='gemini_cli'\nbase_url='https://cloudcode-pa.googleapis.com'\n[[providers]]\nname='vertex'\nkind='gemini'\nauth='google_vertex'\nbase_url='https://us-central1-aiplatform.googleapis.com'\nproject_id='test-project'\nregion='us-central1'").unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(&cfg, db, None, OAuthEndpoints::default())
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, tokomotive::router(st)).await.unwrap() });
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("node")
            .arg(script)
            .arg(url)
            .status()
    })
    .await
    .unwrap()
    .unwrap();
    server.abort();
    assert!(result.success());
}

#[test]
fn code_assist_is_a_fixed_gemini_subscription() {
    let auth: ProviderAuth = serde_json::from_value(json!("gemini_cli")).unwrap();
    assert!(auth.is_subscription());
    let cfg: Config = toml::from_str("[[providers]]\nname='cli'\nkind='gemini'\nauth='gemini_cli'\nbase_url='https://cloudcode-pa.googleapis.com'").unwrap();
    cfg.validate().unwrap();
    let mut bad = cfg.clone();
    bad.providers[0].base_url = "https://attacker.cloudcode-pa.googleapis.com".into();
    assert!(bad.validate().is_err());
}

#[tokio::test]
async fn code_assist_proxy_envelopes_json_and_sse_and_enforces_access() {
    use axum::{Json, Router, response::IntoResponse, routing::post};
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    let mock=Router::new()
        .route("/token",post(||async {Json(json!({"access_token":"private","refresh_token":"refresh","expires_in":3600}))}))
        .route("/v1internal:loadCodeAssist",post(||async {Json(json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"managed"}))}))
        .route("/{*path}",post(|uri:axum::http::Uri,h:axum::http::HeaderMap,Json(v):Json<serde_json::Value>|async move {
            assert_eq!(h["authorization"],"Bearer private");
            assert_eq!(v["model"],"gemini-2.5-pro");assert_eq!(v["project"],"managed");
            assert_eq!(v["request"]["contents"][0]["parts"][0]["text"],"hello");
            assert!(v["request"].get("model").is_none());
            let body=json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"cachedContentTokenCount":2,"candidatesTokenCount":3},"modelVersion":"gemini-2.5-pro"}});
            if uri.path().ends_with(":streamGenerateContent") {assert_eq!(uri.query(),Some("alt=sse"));([("content-type","text/event-stream")],format!("data: {body}\n\n")).into_response()}
            else {assert_eq!(uri.path(),"/v1internal:generateContent");Json(body).into_response()}
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(l, mock).await.unwrap() });
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='cli'\nkind='gemini'\nauth='gemini_cli'\nbase_url='https://cloudcode-pa.googleapis.com'\n[[models]]\nname='gemini-2.5-pro'\nprovider='cli'\ncost={input=1,output=1}\n[[users]]\nname='u'\nkeys=['allowed']\nkey_allowed_providers=['cli']\n[[users]]\nname='blocked'\nkeys=['blocked']").unwrap();
    cfg.validate().unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db.clone(),
        None,
        OAuthEndpoints {
            google_token: format!("{base}/token"),
            code_assist_resource: base,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a = st.oauth.start("cli", Flow::GeminiCli).await.unwrap();
    let u = reqwest::Url::parse(&a.authorize_url).unwrap();
    let state = u
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    st.oauth
        .finish("cli", Flow::GeminiCli, &format!("code#{state}"))
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let state = st.clone();
    let server =
        tokio::spawn(async move { axum::serve(l, tokomotive::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    for stream in [false, true] {
        let r=http.post(format!("{url}/v1/chat/completions")).bearer_auth("allowed").json(&json!({"model":"gemini-2.5-pro","stream":stream,"messages":[{"role":"user","content":"hello"}]})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(text.contains("answer"), "{text}");
    }
    st.flush().await;
    let rows: Vec<(i64, i64, i64, i64)> =
        sqlx::query_as("SELECT input,output,cache_read,cost_micros FROM ledger")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(rows, vec![(10, 3, 2, 0), (10, 3, 2, 0)]);
    for (key, path) in [
        ("blocked", "/v1/chat/completions"),
        ("allowed", "/evil/chat/completions"),
    ] {
        let r = http
            .post(format!("{url}{path}"))
            .bearer_auth(key)
            .json(&json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":"hello"}]}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error(), "{key} {path}: {}", r.status());
    }
    server.abort();
    mock.abort();
}

#[tokio::test]
async fn code_assist_signin_onboards_and_refreshes_once_without_rotation() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app = Router::new()
        .route("/token", post(move |axum::Form(f): axum::Form<HashMap<String,String>>| {let seen=seen.clone(); async move {
            assert_eq!(f["client_id"], "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com");
            assert_eq!(f["client_secret"], "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");
            let n=seen.fetch_add(1, Ordering::SeqCst);
            if n==0 {assert_eq!(f["redirect_uri"], "https://codeassist.google.com/authcode"); assert!(!f["code_verifier"].is_empty()); Json(json!({"access_token":"first","refresh_token":"stable-refresh","expires_in":3600}))}
            else {assert_eq!(f["refresh_token"], "stable-refresh"); Json(json!({"access_token":"renewed","expires_in":3600}))}
        }}))
        .route("/v1internal:loadCodeAssist", post(|| async {Json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}))}))
        .route("/v1internal:onboardUser", post(|Json(v):Json<serde_json::Value>| async move {assert_eq!(v["tierId"],"free-tier");assert!(v.get("cloudaicompanionProject").is_none());Json(json!({"name":"operations/setup","done":false}))}))
        .route("/v1internal/operations/setup",get(||async {Json(json!({"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}))}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('cli','\"gemini\"','https://cloudcode-pa.googleapis.com','gemini_cli')").execute(&db).await.unwrap();
    let auth = Arc::new(
        OAuthManager::new(
            db.clone(),
            SecretKey::from_bytes([1; 32]),
            OAuthEndpoints {
                google_token: format!("{base}/token"),
                code_assist_resource: base,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let start = auth.start("cli", Flow::GeminiCli).await.unwrap();
    let url = reqwest::Url::parse(&start.authorize_url).unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        params["redirect_uri"],
        "https://codeassist.google.com/authcode"
    );
    assert_eq!(
        params["scope"],
        "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile"
    );
    assert!(!start.authorize_url.contains("GOCSPX"));
    assert!(auth.start_device("cli", Flow::GeminiCli).await.is_err());
    auth.finish("cli", Flow::GeminiCli, &format!("code#{}", params["state"]))
        .await
        .unwrap();
    assert_eq!(
        auth.status("cli", Flow::GeminiCli)
            .await
            .unwrap()
            .plan
            .as_deref(),
        Some("free-tier")
    );
    assert_eq!(
        auth.code_assist_project("cli").await.unwrap(),
        "managed-project"
    );
    let a = auth.clone();
    let b = auth.clone();
    let (a, b) = tokio::join!(
        a.token("cli", Flow::GeminiCli, Some("first")),
        b.token("cli", Flow::GeminiCli, Some("first"))
    );
    assert_eq!(a.unwrap(), "renewed");
    assert_eq!(b.unwrap(), "renewed");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let encrypted: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!encrypted.contains("stable-refresh"));
    server.abort();
}
