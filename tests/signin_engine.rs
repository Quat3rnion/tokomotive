use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};
use tokomotive::{
    db,
    oauth::{ClientOptions, Flow, OAuthEndpoints, OAuthManager},
    settings::SecretKey,
};

#[tokio::test]
async fn registered_device_client_handles_pending_slow_down_and_refresh() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app = Router::new()
        .route("/device", post(|| async { Json(json!({"device_code":"private-device", "user_code":"PUBLIC", "verification_uri":"https://huggingface.co/activate", "interval":1, "expires_in":900})) }))
        .route("/token", post(move |axum::Form(f): axum::Form<std::collections::HashMap<String,String>>| { let seen=seen.clone(); async move {
            assert_eq!(f["client_id"], "operator-client");
            if f["grant_type"] == "refresh_token" { return (StatusCode::OK,Json(json!({"access_token":"refreshed", "expires_in":3600}))); }
            assert_eq!(f["device_code"], "private-device");
            match seen.fetch_add(1,Ordering::SeqCst) {
                0 => (StatusCode::BAD_REQUEST,Json(json!({"error":"authorization_pending"}))),
                1 => (StatusCode::BAD_REQUEST,Json(json!({"error":"slow_down"}))),
                _ => (StatusCode::OK,Json(json!({"access_token":"hf-token", "refresh_token":"hf-refresh", "expires_in":3600, "scope":"inference-api"})))
            }
        }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('hf','\"openai\"','https://router.huggingface.co','hugging_face')").execute(&db).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let auth = OAuthManager::new(
        db.clone(),
        SecretKey::from_bytes([4; 32]),
        OAuthEndpoints {
            hf_device: format!("{base}/device"),
            hf_token: format!("{base}/token"),
            ..Default::default()
        },
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    auth.configure(
        "hf",
        Flow::HuggingFace,
        ClientOptions {
            client_id: "operator-client".into(),
            client_secret: "client-secret".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a = auth.start_device("hf", Flow::HuggingFace).await.unwrap();
    assert!(
        !serde_json::to_string(&a)
            .unwrap()
            .contains("private-device")
    );
    for now in [2000, 3000, 9000] {
        clock.store(now, Ordering::SeqCst);
        assert_eq!(
            auth.poll_device("hf", Flow::HuggingFace, &a.attempt)
                .await
                .unwrap(),
            now != 9000
        );
    }
    assert_eq!(
        auth.device_status("hf", Flow::HuggingFace, &a.attempt)
            .await
            .unwrap()
            .state,
        "connected"
    );
    assert_eq!(
        auth.token("hf", Flow::HuggingFace, None).await.unwrap(),
        "hf-token"
    );
    assert_eq!(
        auth.token("hf", Flow::HuggingFace, Some("hf-token"))
            .await
            .unwrap(),
        "refreshed"
    );
    let stored: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!stored.contains("client-secret"));
    assert!(!stored.contains("hf-token"));
    server.abort();
}

#[tokio::test]
async fn engine_uses_configuration_for_optional_device_pkce_and_inference_derivation() {
    use axum::routing::get;
    use tokomotive::oauth::{FlowConfig, FlowType, InferenceBase, RefreshRule, TokenEncoding};
    let app=Router::new()
        .route("/device",post(|axum::Form(v):axum::Form<std::collections::HashMap<String,String>>|async move {
            assert_eq!(v["code_challenge_method"],"S256");assert_eq!(v["code_challenge"].len(),43);
            Json(json!({"device_code":"private","user_code":"PUBLIC","verification_uri":"https://issuer.example/device","interval":1}))
        }))
        .route("/token",post(|axum::Form(v):axum::Form<std::collections::HashMap<String,String>>|async move {
            assert!(v["code_verifier"].len()>=43);
            Json(json!({"access_token":"custom-access","expires_in":3600,"api_base":"https://inference.example/v1"}))
        }))
        .route("/resource",get(|h:axum::http::HeaderMap|async move {
            assert_eq!(h["authorization"],"Bearer custom-access");
            Json(json!({"base":"https://inference.example/v1"}))
        }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('custom','\"openai\"','https://router.huggingface.co','hugging_face')").execute(&db).await.unwrap();
    for inference in [
        InferenceBase::TokenField {
            field: "api_base".into(),
            origin: "https://inference.example".into(),
        },
        InferenceBase::FollowUp {
            url: format!("{base}/resource"),
            field: "base".into(),
            origin: "https://inference.example".into(),
        },
    ] {
        let clock = Arc::new(AtomicI64::new(1000));
        let c = clock.clone();
        let config = FlowConfig {
            flow_type: FlowType::DeviceCode,
            authorize: format!("{base}/authorize"),
            device: Some(format!("{base}/device")),
            token: format!("{base}/token"),
            client_id: "configured-client".into(),
            client_secret: None,
            scopes: "inference-api".into(),
            authorize_params: vec![],
            token_encoding: TokenEncoding::Form,
            headers: vec![],
            inference_base: inference,
            refresh: RefreshRule::None,
            redirect: "http://localhost/auth/callback",
            instructions: "Paste the redirect URL.",
            device_pkce: true,
            basic_secret: false,
        };
        let auth = OAuthManager::new(
            db.clone(),
            SecretKey::from_bytes([9; 32]),
            OAuthEndpoints::default(),
        )
        .unwrap()
        .with_flow_config(Flow::HuggingFace, config)
        .with_clock(move || c.load(Ordering::SeqCst));
        let a = auth
            .start_device("custom", Flow::HuggingFace)
            .await
            .unwrap();
        clock.store(2000, Ordering::SeqCst);
        assert!(
            !auth
                .poll_device("custom", Flow::HuggingFace, &a.attempt)
                .await
                .unwrap()
        );
        assert_eq!(
            auth.token("custom", Flow::HuggingFace, None).await.unwrap(),
            "custom-access"
        );
        assert_eq!(
            auth.inference_resource(
                "custom",
                tokomotive::provider::ProviderAuth::HuggingFace,
                "unused"
            )
            .await
            .unwrap(),
            "https://inference.example/v1"
        );
    }
    server.abort();
}

#[tokio::test]
async fn paid_device_bearers_reach_only_their_inference_base_and_model_paths() {
    use tokomotive::{config::Config, state::AppState};
    for (auth, flow, resource) in [
        (
            "hugging_face",
            Flow::HuggingFace,
            "https://router.huggingface.co",
        ),
        (
            "entra",
            Flow::Entra,
            "https://example.openai.azure.com/openai",
        ),
    ] {
        let reached = Arc::new(AtomicUsize::new(0));
        let seen = reached.clone();
        let app=Router::new()
            .route("/tenant/oauth2/v2.0/devicecode",post(|axum::Form(f):axum::Form<std::collections::HashMap<String,String>>|async move {
                assert_eq!(f["scope"],"https://cognitiveservices.azure.com/.default offline_access");
                Json(json!({"device_code":"private-device","user_code":"PUBLIC","verification_uri":"https://microsoft.com/devicelogin","interval":1}))
            }))
            .route("/token",post(||async{Json(json!({"access_token":"paid-device-bearer","refresh_token":"refresh","expires_in":3600}))}))
            .route("/tenant/oauth2/v2.0/token",post(||async{Json(json!({"access_token":"paid-device-bearer","refresh_token":"refresh","expires_in":3600}))}))
            .route("/v1/chat/completions",post(move |h:axum::http::HeaderMap| { let seen=seen.clone();async move {
                assert_eq!(h["authorization"],"Bearer paid-device-bearer"); assert!(!h.contains_key("api-key"));
                seen.fetch_add(1,Ordering::SeqCst);
                Json(json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1}}))
            }}))
            .fallback(|| async {
                fn unexpected_path() -> StatusCode { panic!("Bearer reached a non-model path") }
                unexpected_path()
            });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let issuer = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let cfg:Config=toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='paid'\nkind='openai'\nauth='{auth}'\nbase_url='{resource}'\n[[models]]\nname='m'\nprovider='paid'\ncost={{input=2,output=4}}\n[[users]]\nname='user'\nkeys=['vk-paid']\nkey_allowed_providers=['paid']\n[[users]]\nname='no-grant'\nkeys=['vk-denied']")).unwrap();
        cfg.validate().unwrap();
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::seed(&db, &cfg).await.unwrap();
        let endpoints = OAuthEndpoints {
            hf_token: format!("{base}/token"),
            hf_resource: base.clone(),
            entra_issuer: base.clone(),
            entra_resource: Some(base.clone()),
            ..Default::default()
        };
        let st = AppState::new_with_oauth_endpoints(&cfg, db, None, endpoints)
            .await
            .unwrap();
        st.oauth
            .configure(
                "paid",
                flow,
                ClientOptions {
                    client_id: "operator-client".into(),
                    tenant: "tenant".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Paste is the common fallback for both registered device clients.
        let a = st.oauth.start("paid", flow).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        st.oauth
            .finish("paid", flow, &format!("code#{}", q["state"]))
            .await
            .unwrap();
        if flow == Flow::Entra {
            let a = st.oauth.start_device("paid", flow).await.unwrap();
            assert_eq!(a.user_code, "PUBLIC");
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let app = tokomotive::router(st.clone());
        let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let c = reqwest::Client::new();
        for (path, key, expected) in [
            ("/v1/chat/completions", "vk-paid", 200),
            ("/v1/chat/completions", "vk-denied", 403),
            ("/v1/organizations", "vk-paid", 400),
            ("/v1/chat/completions/extra", "vk-paid", 400),
        ] {
            let r = c
                .post(format!("{url}{path}"))
                .bearer_auth(key)
                .json(&json!({"model":"m","messages":[]}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), expected, "{auth} {path}");
            let _ = r.bytes().await.unwrap();
        }
        st.flush().await;
        assert_eq!(reached.load(Ordering::SeqCst), 1);
        let row: (i64, i64) =
            sqlx::query_as("SELECT cost_micros,subscription FROM ledger WHERE status=200")
                .fetch_one(&st.db)
                .await
                .unwrap();
        assert_eq!(row, (8, 0));
        server.abort();
        issuer.abort();
    }
}

#[tokio::test]
async fn operator_configures_google_secret_then_pastes_pkce_with_routing_available() {
    use tokomotive::{config::Config, provider::Endpoint, state::AppState};
    let app=Router::new().route("/token",post(|axum::Form(f):axum::Form<std::collections::HashMap<String,String>>|async move {
        assert_eq!(f["client_secret"],"google-private-secret");
        assert_eq!(f["client_id"],"google-owned-client");
        Json(json!({"access_token":"google-access-private","refresh_token":"google-refresh-private","expires_in":3600}))
    })).route("/key",post(|| async { Json(json!({"key":"issued-private-KEY4"})) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let issuer = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='vertex'\nkind='openai'\nauth='google_vertex'\nbase_url='https://aiplatform.googleapis.com'\n[[providers]]\nname='or'\nkind='openai'\nbase_url='https://openrouter.ai/api'\n[[providers]]\nname='hf'\nkind='openai'\nauth='hugging_face'\nbase_url='https://router.huggingface.co'\n[[providers]]\nname='azure'\nkind='openai'\nauth='entra'\nbase_url='https://example.openai.azure.com/openai'").unwrap();
    cfg.validate().unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            google_token: format!("{base}/token"),
            openrouter_token: format!("{base}/key"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
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
        .post(format!("{url}/admin/providers/vertex/oauth/configure"))
        .header("cookie", &cookie)
        .form(&[
            ("client_id", ""),
            ("client_secret", "never-echo-invalid-secret"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(
        !r.text()
            .await
            .unwrap()
            .contains("never-echo-invalid-secret")
    );
    let r = c
        .post(format!("{url}/admin/providers/vertex/oauth/configure"))
        .header("cookie", &cookie)
        .form(&[
            ("client_id", "google-owned-client"),
            ("client_secret", "google-private-secret"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    st.oauth.start("or", Flow::OpenRouter).await.unwrap();
    let r = c
        .post(format!("{url}/admin/providers/or/oauth/finish"))
        .header("cookie", &cookie)
        .form(&[("callback", "headless-code")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(
        st.router_table()
            .providers
            .iter()
            .find(|p| p.name == "or")
            .unwrap()
            .keys[0],
        "issued-private-KEY4"
    );
    let a = st.oauth.start("vertex", Flow::GoogleVertex).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(q["scope"], "https://www.googleapis.com/auth/cloud-platform");
    assert_eq!(q["access_type"], "offline");
    st.oauth
        .finish(
            "vertex",
            Flow::GoogleVertex,
            &format!(
                "http://localhost/auth/callback?code=google-code&state={}",
                q["state"]
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        st.oauth
            .token("vertex", Flow::GoogleVertex, Some("google-access-private"))
            .await
            .unwrap(),
        "google-access-private"
    );
    assert!(
        !st.candidates(
            &st.router_table(),
            &st.catalog(),
            Some("vertex/model"),
            Endpoint::ChatCompletions,
            false
        )
        .is_empty()
    );
    assert!(
        st.candidates(
            &tokomotive::routing::RouterTable::from_config(&cfg),
            &st.catalog(),
            None,
            Endpoint::ChatCompletions,
            false
        )
        .is_empty()
    );
    let html = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Sign in with OpenRouter"));
    assert!(!html.contains("adapter pending"));
    assert!(html.contains("Service-account JSON key"));
    assert!(html.contains("registering an app"));
    assert!(html.contains("****KEY4"));
    assert!(!html.contains("issued-private-KEY4"));
    for secret in [
        "google-private-secret",
        "google-access-private",
        "google-refresh-private",
        "google-owned-client",
    ] {
        assert!(!html.contains(secret));
    }
    let stored: String =
        sqlx::query_scalar("SELECT secret FROM oauth_credentials WHERE provider='vertex'")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(!stored.contains("google-private-secret"));
    st.oauth
        .configure(
            "hf",
            Flow::HuggingFace,
            ClientOptions {
                client_id: "https://proxy.example/.well-known/oauth-cimd".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r = c
        .get(format!("{url}/.well-known/oauth-cimd"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let metadata: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        metadata["client_id"],
        "https://proxy.example/.well-known/oauth-cimd"
    );
    assert_eq!(metadata["token_endpoint_auth_method"], "none");
    assert_eq!(
        metadata["redirect_uris"][0],
        "http://localhost/auth/callback"
    );
    let a = st.oauth.start("hf", Flow::HuggingFace).await.unwrap();
    assert!(a.authorize_url.contains("code_challenge_method=S256"));
    server.abort();
    issuer.abort();
}

#[tokio::test]
async fn pkce_key_becomes_an_encrypted_normal_provider_key() {
    let app = Router::new().route(
        "/key",
        post(|Json(v): Json<serde_json::Value>| async move {
            assert_eq!(v["code"], "headless-code");
            assert_eq!(v["code_challenge_method"], "S256");
            assert!(v["code_verifier"].as_str().unwrap().len() >= 43);
            assert!(v.get("client_secret").is_none());
            Json(json!({"key":"sk-or-private-issued-KEY4"}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('or','\"openai\"','https://openrouter.ai/api','api_key')").execute(&db).await.unwrap();
    let key = SecretKey::from_bytes([5; 32]);
    let auth = OAuthManager::new(
        db.clone(),
        key.clone(),
        OAuthEndpoints {
            openrouter_token: format!("{base}/key"),
            ..Default::default()
        },
    )
    .unwrap();
    let a = auth.start("or", Flow::OpenRouter).await.unwrap();
    assert!(!a.authorize_url.contains("callback_url"));
    auth.finish("or", Flow::OpenRouter, "headless-code")
        .await
        .unwrap();
    let row: (String, String) =
        sqlx::query_as("SELECT auth,keys_json FROM providers WHERE name='or'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(row.0, "api_key");
    assert!(!row.1.contains("sk-or-private"));
    let keys: Vec<String> = serde_json::from_str(&row.1).unwrap();
    assert_eq!(key.decrypt(&keys[0]).unwrap(), "sk-or-private-issued-KEY4");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM oauth_credentials")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    server.abort();
}

/// Run the browser script against local issuers, without account credentials.
#[tokio::test]
#[ignore = "Playwright fixture; set TOKOMOTIVE_PLAYWRIGHT to a local script"]
async fn official_signin_panels_in_browser() {
    use tokomotive::{config::Config, state::AppState};
    let script = std::env::var("TOKOMOTIVE_PLAYWRIGHT").expect("Set TOKOMOTIVE_PLAYWRIGHT");
    let app=Router::new()
        .route("/device",post(||async{Json(json!({"device_code":"never-show-device-private","user_code":"DEMO-CODE","verification_uri":"https://huggingface.co/activate","expires_in":900,"interval":900}))}))
        .route("/tenant/oauth2/v2.0/devicecode",post(||async{Json(json!({"device_code":"never-show-entra-private","user_code":"AZURE-CODE","verification_uri":"https://microsoft.com/devicelogin","expires_in":900,"interval":900}))}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='vertex'\nkind='openai'\nauth='google_vertex'\nbase_url='https://aiplatform.googleapis.com'\n[[providers]]\nname='openrouter'\nkind='openai'\nbase_url='https://openrouter.ai/api'\n[[providers]]\nname='huggingface'\nkind='openai'\nauth='hugging_face'\nbase_url='https://router.huggingface.co'\n[[providers]]\nname='azure'\nkind='openai'\nauth='entra'\nbase_url='https://example.openai.azure.com/openai'").unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            hf_device: format!("{base}/device"),
            entra_issuer: base,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for (name, flow) in [
        ("huggingface", Flow::HuggingFace),
        ("azure", Flow::Entra),
        ("vertex", Flow::GoogleVertex),
    ] {
        st.oauth
            .configure(
                name,
                flow,
                ClientOptions {
                    client_id: "fixture-client".into(),
                    client_secret: if flow == Flow::GoogleVertex {
                        "fixture-secret-private".into()
                    } else {
                        String::new()
                    },
                    tenant: "tenant".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, tokomotive::router(st)).await.unwrap() });
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
    mock.abort();
    assert!(result.success());
}
