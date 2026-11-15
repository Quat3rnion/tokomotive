use tokomotive::vertex::{base_url, model_path};

fn service_key() -> (String, String) {
    use rsa::{
        RsaPrivateKey,
        pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding},
    };
    use std::sync::OnceLock;
    static KEY: OnceLock<(String, String)> = OnceLock::new();
    KEY.get_or_init(|| {
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        (
            key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string(),
            key.to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap(),
        )
    })
    .clone()
}

#[tokio::test]
async fn vertex_routes_gemini_and_claude_json_and_stream_with_normal_cost() {
    use axum::{Json, Router, response::IntoResponse, routing::post};
    use serde_json::{Value, json};
    use tokomotive::{config::Config, db, oauth::OAuthEndpoints, state::AppState};
    let mock=Router::new().route("/token",post(||async {Json(json!({"access_token":"vertex-private","expires_in":3600}))}))
        .route("/{*path}",post(|uri:axum::http::Uri,h:axum::http::HeaderMap,Json(v):Json<Value>|async move {
            assert_eq!(h["authorization"],"Bearer vertex-private");assert!(v.get("model").is_none());
            if h.contains_key("x-mock-redirect") { return (axum::http::StatusCode::TEMPORARY_REDIRECT,[("location","/redirect-trap")]).into_response(); }
            if uri.path().contains("/publishers/anthropic/") {
                assert_eq!(v["anthropic_version"],"vertex-2023-10-16");assert_eq!(v["messages"][0]["content"][0]["text"],"hello");
                if uri.path().ends_with(":streamRawPredict") {
                    assert_eq!(v["stream"],true);assert!(uri.query().is_none());
                    let events=[json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"claude-sonnet-4-5@20250929","content":[],"usage":{"input_tokens":10,"cache_read_input_tokens":2,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),json!({"type":"message_stop"})];
                    return ([("content-type","text/event-stream")],events.into_iter().map(|v|format!("event: {}\ndata: {v}\n\n",v["type"].as_str().unwrap())).collect::<String>()).into_response();
                }
                assert!(uri.path().ends_with("/claude-sonnet-4-5@20250929:rawPredict"));
                Json(json!({"id":"m","type":"message","role":"assistant","model":"claude-sonnet-4-5@20250929","content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":2,"output_tokens":3}})).into_response()
            } else {
                assert!(uri.path().contains("/projects/test-project/locations/us-central1/publishers/google/models/gemini-2.5-pro:"));
                assert_eq!(v["contents"][0]["parts"][0]["text"],"hello");
                let mut reply=json!({"modelVersion":"gemini-2.5-pro","candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"cachedContentTokenCount":2,"candidatesTokenCount":3}});
                if h.contains_key("x-mock-no-model") { reply.as_object_mut().unwrap().remove("modelVersion"); }
                if uri.path().ends_with(":streamGenerateContent") {assert_eq!(uri.query(),Some("alt=sse")); ([("content-type","text/event-stream")],format!("data: {reply}\n\n")).into_response()} else {assert!(uri.path().ends_with(":generateContent"));Json(reply).into_response()}
            }
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(l, mock).await.unwrap() });
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='vertex'\nkind='gemini'\nauth='google_vertex'\nbase_url='https://us-central1-aiplatform.googleapis.com'\nproject_id='test-project'\nregion='us-central1'\n[[models]]\nname='gemini-2.5-pro'\nprovider='vertex'\ncost={input=1,output=1,cache_read=1}\n[[models]]\nname='claude-sonnet-4-5@20250929'\nprovider='vertex'\ncost={input=1,output=1,cache_read=1}\n[[users]]\nname='u'\nkeys=['allowed']\nkey_allowed_providers=['vertex']").unwrap();
    cfg.validate().unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db.clone(),
        None,
        OAuthEndpoints {
            google_token: format!("{base}/token"),
            vertex_resource: Some(base),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (private, _) = service_key();
    st.oauth.configure_service_account("vertex",&json!({"type":"service_account","client_email":"router@test-project.iam.gserviceaccount.com","private_key_id":"test-key","private_key":private,"token_uri":"https://oauth2.googleapis.com/token"}).to_string()).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let state = st.clone();
    let server =
        tokio::spawn(async move { axum::serve(l, tokomotive::router(state)).await.unwrap() });
    for endpoint in ["chat/completions", "messages"] {
        for model in ["gemini-2.5-pro", "claude-sonnet-4-5@20250929"] {
            for stream in [false, true] {
                let r=reqwest::Client::new().post(format!("{url}/v1/{endpoint}")).bearer_auth("allowed").json(&json!({"model":model,"stream":stream,"max_tokens":50,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]})).send().await.unwrap();
                assert_eq!(r.status(), 200, "{endpoint} {model} {stream}");
                let body = r.text().await.unwrap();
                assert!(body.contains("answer"), "{body}");
            }
        }
    }
    let body = json!({"model":"vertex/google/gemini-2.5-pro","max_tokens":50,"messages":[{"role":"user","content":"hello"}]});
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("allowed")
        .header("x-mock-no-model", "1")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.text().await.unwrap();
    st.flush().await;
    let rows: Vec<(i64, i64, i64, i64, i64)> =
        sqlx::query_as("SELECT input,output,cache_read,cost_micros,subscription FROM ledger")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(rows, vec![(10, 3, 2, 15, 0); 9]);
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("allowed")
        .header("x-mock-redirect", "1")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    for (method, path) in [
        (reqwest::Method::POST, "/evil/chat/completions"),
        (reqwest::Method::GET, "/v1/messages"),
    ] {
        let r = reqwest::Client::new()
            .request(method, format!("{url}{path}"))
            .bearer_auth("allowed")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error());
    }
    server.abort();
    mock.abort();
}

#[tokio::test]
async fn service_account_signs_rs256_caches_and_renews_an_encrypted_credential() {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicI64, AtomicUsize, Ordering},
        },
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let (private, public) = service_key();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app=Router::new().route("/token",post(move |axum::Form(f):axum::Form<HashMap<String,String>>|{let public=public.clone();let seen=seen.clone();async move {
        assert_eq!(f["grant_type"],"urn:ietf:params:oauth:grant-type:jwt-bearer");
        let header=jsonwebtoken::decode_header(&f["assertion"]).unwrap();assert_eq!(header.alg,jsonwebtoken::Algorithm::RS256);assert_eq!(header.kid.as_deref(),Some("test-key"));
        let mut validation=jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);validation.set_audience(&["https://oauth2.googleapis.com/token"]);validation.validate_exp=false;
        let claims=jsonwebtoken::decode::<Value>(&f["assertion"],&jsonwebtoken::DecodingKey::from_rsa_pem(public.as_bytes()).unwrap(),&validation).unwrap().claims;
        assert_eq!(claims["iss"],"router@test-project.iam.gserviceaccount.com");assert_eq!(claims["scope"],"https://www.googleapis.com/auth/cloud-platform");assert_eq!(claims["exp"].as_i64().unwrap()-claims["iat"].as_i64().unwrap(),3600);
        assert!(claims.get("sub").is_none());
        let n=seen.fetch_add(1,Ordering::SeqCst);Json(json!({"access_token":format!("service-{n}"),"expires_in":3600,"token_type":"Bearer"}))
    }}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('v','\"gemini\"','https://aiplatform.googleapis.com','google_vertex')").execute(&db).await.unwrap();
    let now = Arc::new(AtomicI64::new(1_700_000_000_000));
    let clock = now.clone();
    let oauth = Arc::new(
        OAuthManager::new(
            db.clone(),
            SecretKey::from_bytes([1; 32]),
            OAuthEndpoints {
                google_token: format!("{base}/token"),
                ..Default::default()
            },
        )
        .unwrap()
        .with_clock(move || clock.load(Ordering::SeqCst)),
    );
    let key = json!({"type":"service_account","client_email":"router@test-project.iam.gserviceaccount.com","private_key_id":"test-key","private_key":private,"token_uri":"https://oauth2.googleapis.com/token"});
    oauth
        .configure_service_account("v", &key.to_string())
        .await
        .unwrap();
    assert_eq!(
        oauth.token("v", Flow::GoogleVertex, None).await.unwrap(),
        "service-0"
    );
    assert_eq!(
        oauth.token("v", Flow::GoogleVertex, None).await.unwrap(),
        "service-0"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    now.fetch_add(3_000_000, Ordering::SeqCst);
    oauth.refresh_due_credentials().await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "service-account background refresh before expiry"
    );
    let (a, b) = tokio::join!(
        oauth.token("v", Flow::GoogleVertex, None),
        oauth.token("v", Flow::GoogleVertex, None)
    );
    assert_eq!(a.unwrap(), "service-1");
    assert_eq!(b.unwrap(), "service-1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let encrypted: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!encrypted.contains("PRIVATE KEY"));
    assert!(!encrypted.contains("service-1"));
    let mut bad = key;
    bad["token_uri"] = json!("https://attacker.example/token");
    assert!(
        oauth
            .configure_service_account("v", &bad.to_string())
            .await
            .is_err()
    );
    oauth.disconnect("v", Flow::GoogleVertex).await.unwrap();
    assert!(oauth.token("v", Flow::GoogleVertex, None).await.is_err());
    server.abort();
}

#[test]
fn vertex_urls_select_publisher_and_reject_path_or_host_injection() {
    assert_eq!(
        base_url("global").unwrap(),
        "https://aiplatform.googleapis.com"
    );
    assert_eq!(
        base_url("us-central1").unwrap(),
        "https://us-central1-aiplatform.googleapis.com"
    );
    assert_eq!(
        model_path("test-project", "global", "gemini-2.5-pro", false).unwrap(),
        "/v1/projects/test-project/locations/global/publishers/google/models/gemini-2.5-pro:generateContent"
    );
    assert_eq!(
        model_path(
            "test-project",
            "us-east5",
            "claude-sonnet-4-5@20250929",
            false
        )
        .unwrap(),
        "/v1/projects/test-project/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929:rawPredict"
    );
    assert_eq!(
        model_path("test-project", "global", "anthropic/custom-model", true).unwrap(),
        "/v1/projects/test-project/locations/global/publishers/anthropic/models/custom-model:streamRawPredict"
    );
    assert_eq!(
        model_path("test-project", "europe-west4", "google/claude-custom", true).unwrap(),
        "/v1/projects/test-project/locations/europe-west4/publishers/google/models/claude-custom:streamGenerateContent?alt=sse"
    );
    for region in [
        "",
        "GLOBAL",
        "us-central1.evil",
        "../global",
        "us/central1",
        "us-central1:443",
        "us-central1?x",
        "us",
    ] {
        assert!(base_url(region).is_err(), "{region}");
    }
    for project in [
        "",
        "abc",
        "Test-project",
        "../project",
        "test/project",
        "test-project?x",
        "test-project.",
        "test-project-",
    ] {
        assert!(
            model_path(project, "global", "gemini", false).is_err(),
            "{project}"
        );
    }
    for model in [
        "../model",
        "google/../model",
        "m?key=x",
        "m#x",
        "evil/model",
        "google/a/b",
        "m:rawPredict",
    ] {
        assert!(
            model_path("test-project", "global", model, false).is_err(),
            "{model}"
        );
    }
}
