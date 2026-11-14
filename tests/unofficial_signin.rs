use tokomotive::provider::ProviderAuth;

#[test]
fn unofficial_signins_are_subscriptions() {
    for name in ["github_copilot", "qwen_code", "kimi_code"] {
        let auth: ProviderAuth = serde_json::from_value(serde_json::json!(name)).unwrap();
        assert!(auth.is_subscription(), "{name}");
        assert_eq!(auth.as_str(), name);
    }
}

#[tokio::test]
async fn copilot_and_kimi_use_the_selected_responses_or_messages_protocol() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use tokomotive::{
        config::Config,
        db,
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    for (name, flow, kind, client_path, upstream_path) in [
        (
            "github_copilot",
            Flow::GithubCopilot,
            "openai",
            "/v1/responses",
            "responses",
        ),
        (
            "github_copilot",
            Flow::GithubCopilot,
            "anthropic",
            "/v1/messages",
            "v1/messages",
        ),
        (
            "kimi_code",
            Flow::KimiCode,
            "openai",
            "/v1/responses",
            "coding/v1/responses",
        ),
        (
            "kimi_code",
            Flow::KimiCode,
            "anthropic",
            "/v1/messages",
            "coding/v1/messages",
        ),
    ] {
        let app=Router::new()
            .route("/device",post(||async{Json(json!({"device_code":"d","user_code":"U","verification_uri":"https://issuer.example","interval":1}))}))
            .route("/token",post(||async{Json(json!({"access_token":"protocol-private","refresh_token":"refresh","expires_in":3600}))}))
            .route("/user",get(||async{Json(json!({"chat_enabled":true}))}))
            .route("/{*path}",post(move |axum::extract::Path(path):axum::extract::Path<String>,uri:axum::http::Uri,h:axum::http::HeaderMap|async move{
                assert_eq!(path,upstream_path);assert_eq!(h["authorization"],"Bearer protocol-private");
                if flow==Flow::KimiCode && kind=="anthropic" {assert_eq!(uri.query(),Some("beta=true"));}
                if flow==Flow::GithubCopilot {assert_eq!(h["x-initiator"],"user");assert_eq!(h["copilot-vision-request"],"true");}
                if kind=="anthropic" {assert_eq!(h["anthropic-version"],"2023-06-01");Json(json!({"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":2,"output_tokens":1}}))}
                else {Json(json!({"id":"r","object":"response","output":[],"usage":{"input_tokens":2,"output_tokens":1}}))}
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let mock = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let resource = if flow == Flow::GithubCopilot {
            "https://api.githubcopilot.com"
        } else {
            "https://api.kimi.com/coding/v1"
        };
        let cfg:Config=toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='p'\nkind='{kind}'\nauth='{name}'\nbase_url='{resource}'\n[[models]]\nname='m'\nprovider='p'\ncost={{input=1,output=1}}\n[[users]]\nname='u'\nkeys=['vk-protocol']\nkey_allowed_providers=['p']")).unwrap();
        cfg.validate().unwrap();
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::seed(&db, &cfg).await.unwrap();
        let st = AppState::new_with_oauth_endpoints(
            &cfg,
            db,
            None,
            OAuthEndpoints {
                copilot_device: format!("{base}/device"),
                copilot_token: format!("{base}/token"),
                copilot_user: format!("{base}/user"),
                copilot_resource: base.clone(),
                kimi_device: format!("{base}/device"),
                kimi_token: format!("{base}/token"),
                kimi_resource: format!("{base}/coding/v1"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let a = st.oauth.start_device("p", flow).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1010)).await;
        st.oauth.poll_device("p", flow, &a.attempt).await.unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(l, tokomotive::router(st)).await.unwrap() });
        let body = if kind == "anthropic" {
            json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.com/a.png"}}]}]})
        } else {
            json!({"model":"m","input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/a.png"}]}]})
        };
        let r = reqwest::Client::new()
            .post(format!("{url}{client_path}"))
            .bearer_auth("vk-protocol")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{name} {kind}");
        r.bytes().await.unwrap();
        server.abort();
        mock.abort();
    }
}

#[tokio::test]
async fn qwen_refresh_pairs_the_new_token_with_a_validated_resource() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app=Router::new().route("/device",post(||async{Json(json!({"device_code":"d","user_code":"U","verification_uri":"https://chat.qwen.ai","interval":1}))})).route("/token",post(move||{let seen=seen.clone();async move{let n=seen.fetch_add(1,Ordering::SeqCst);Json(json!({"access_token":format!("token-{n}"),"refresh_token":format!("refresh-{n}"),"expires_in":3600,"resource_url":match n{0=>"portal.qwen.ai",1=>"dashscope-us.aliyuncs.com",_=>"https://foreign.example"}}))}}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('q','\"openai\"','https://unused','qwen_code')").execute(&db).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let auth = OAuthManager::new(
        db,
        SecretKey::from_bytes([1; 32]),
        OAuthEndpoints {
            qwen_device: format!("{base}/device"),
            qwen_token: format!("{base}/token"),
            ..Default::default()
        },
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    let a = auth.start_device("q", Flow::QwenCode).await.unwrap();
    clock.store(2000, Ordering::SeqCst);
    auth.poll_device("q", Flow::QwenCode, &a.attempt)
        .await
        .unwrap();
    assert_eq!(
        auth.token("q", Flow::QwenCode, Some("token-0"))
            .await
            .unwrap(),
        "token-1"
    );
    assert_eq!(
        auth.inference_resource("q", ProviderAuth::QwenCode, "")
            .await
            .unwrap(),
        "https://dashscope-us.aliyuncs.com/v1"
    );
    assert!(
        auth.token("q", Flow::QwenCode, Some("token-1"))
            .await
            .is_err()
    );
    assert!(!auth.status("q", Flow::QwenCode).await.unwrap().connected);
    assert!(auth.token("q", Flow::QwenCode, None).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn copilot_discovery_refuses_missing_access_and_foreign_destinations() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    for user in [
        json!({"chat_enabled":false}),
        json!({"chat_enabled":true,"endpoints":{"api":"https://foreign.example"}}),
        json!({"chat_enabled":true,"endpoints":{"api":"http://api.githubcopilot.com"}}),
        // Exact Copilot API hosts only: no other githubcopilot.com subdomain.
        json!({"chat_enabled":true,"endpoints":{"api":"https://evil.githubcopilot.com"}}),
        json!({"chat_enabled":true,"endpoints":{"api":"https://x.api.githubcopilot.com"}}),
        json!({"chat_enabled":true,"endpoints":{"api":"https://githubcopilot.com"}}),
    ] {
        let app=Router::new().route("/device",post(||async{Json(json!({"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","interval":1}))})).route("/token",post(||async{Json(json!({"access_token":"private-copilot"}))})).route("/user",get(move |h:axum::http::HeaderMap|{let user=user.clone();async move{assert_eq!(h["authorization"],"Bearer private-copilot");assert_eq!(h["x-github-api-version"],"2025-04-01");assert_eq!(h["user-agent"],"Tokomotive/0.1.0");Json(user)}}));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let db = db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('p','\"openai\"','https://api.githubcopilot.com','github_copilot')").execute(&db).await.unwrap();
        let clock = Arc::new(AtomicI64::new(1000));
        let c = clock.clone();
        let auth = OAuthManager::new(
            db,
            SecretKey::from_bytes([1; 32]),
            OAuthEndpoints {
                copilot_device: format!("{base}/device"),
                copilot_token: format!("{base}/token"),
                copilot_user: format!("{base}/user"),
                ..Default::default()
            },
        )
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst));
        let a = auth.start_device("p", Flow::GithubCopilot).await.unwrap();
        clock.store(2000, Ordering::SeqCst);
        auth.poll_device("p", Flow::GithubCopilot, &a.attempt)
            .await
            .unwrap();
        assert!(
            !auth
                .status("p", Flow::GithubCopilot)
                .await
                .unwrap()
                .connected
        );
        assert!(auth.token("p", Flow::GithubCopilot, None).await.is_err());
        server.abort();
    }
}

#[tokio::test]
#[ignore = "Playwright fixture; set TOKOMOTIVE_PLAYWRIGHT to a local script"]
async fn unofficial_signin_panels_in_browser() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use tokomotive::{config::Config, db, oauth::OAuthEndpoints, state::AppState};
    let script = std::env::var("TOKOMOTIVE_PLAYWRIGHT").expect("Set TOKOMOTIVE_PLAYWRIGHT");
    let mock=Router::new().route("/device",post(||async{Json(json!({"device_code":"never-show-private-device","user_code":"DEMO-CODE","verification_uri":"https://issuer.example/activate","interval":900,"expires_in":900}))}));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(l, mock).await.unwrap() });
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='copilot'\nkind='openai'\nauth='github_copilot'\nbase_url='https://api.githubcopilot.com'\n[[providers]]\nname='qwen'\nkind='openai'\nauth='qwen_code'\nbase_url='https://dashscope.aliyuncs.com/compatible-mode/v1'\n[[providers]]\nname='kimi'\nkind='openai'\nauth='kimi_code'\nbase_url='https://api.kimi.com/coding/v1'").unwrap();
    cfg.validate().unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        db,
        None,
        OAuthEndpoints {
            copilot_device: format!("{base}/device"),
            qwen_device: format!("{base}/device"),
            kimi_device: format!("{base}/device"),
            ..Default::default()
        },
    )
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
    mock.abort();
    assert!(result.success());
}

#[tokio::test]
async fn qwen_resource_urls_are_normalized_only_for_trusted_https_hosts() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    for (resource, expected) in [
        ("portal.qwen.ai", Some("https://portal.qwen.ai/v1")),
        (
            "https://dashscope.aliyuncs.com/compatible-mode/v1/",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        ),
        (
            "https://dashscope-intl.aliyuncs.com",
            Some("https://dashscope-intl.aliyuncs.com/v1"),
        ),
        ("http://portal.qwen.ai", None),
        ("https://127.0.0.1", None),
        ("https://foreign.example/v1", None),
        ("https://portal.qwen.ai.evil.example", None),
        ("https://portal.qwen.ai@evil.example", None),
        ("https://portal.qwen.ai:8443/v1", None),
        ("https://portal.qwen.ai/v1?redirect=evil", None),
        // Exact hosts only: no wildcard subdomains of a trusted host.
        ("https://attacker.portal.qwen.ai/v1", None),
        ("https://x.dashscope.aliyuncs.com/v1", None),
        ("https://user.maas.aliyuncs.com/v1", None),
        (
            "https://token-plan.maas.aliyuncs.com/v1",
            Some("https://token-plan.maas.aliyuncs.com/v1"),
        ),
    ] {
        let app=Router::new().route("/device",post(||async{Json(json!({"device_code":"d","user_code":"U","verification_uri":"https://chat.qwen.ai","interval":1}))})).route("/token",post(move||async move{Json(json!({"access_token":"private","refresh_token":"refresh","expires_in":3600,"resource_url":resource}))}));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let db = db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('q','\"openai\"','https://unused','qwen_code')").execute(&db).await.unwrap();
        let clock = Arc::new(AtomicI64::new(1000));
        let c = clock.clone();
        let auth = OAuthManager::new(
            db,
            SecretKey::from_bytes([1; 32]),
            OAuthEndpoints {
                qwen_device: format!("{base}/device"),
                qwen_token: format!("{base}/token"),
                ..Default::default()
            },
        )
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst));
        let a = auth.start_device("q", Flow::QwenCode).await.unwrap();
        clock.store(2000, Ordering::SeqCst);
        auth.poll_device("q", Flow::QwenCode, &a.attempt)
            .await
            .unwrap();
        assert_eq!(
            auth.status("q", Flow::QwenCode).await.unwrap().connected,
            expected.is_some(),
            "{resource}"
        );
        if let Some(expected) = expected {
            assert_eq!(
                auth.inference_resource("q", ProviderAuth::QwenCode, "")
                    .await
                    .unwrap(),
                expected
            );
        }
        server.abort();
    }
}

#[tokio::test]
async fn add_provider_lists_unofficial_device_signins() {
    use tokomotive::{config::Config, db, state::AppState};
    let cfg: Config = toml::from_str("[server]\ndatabase='sqlite::memory:'").unwrap();
    let db = db::connect("sqlite::memory:").await.unwrap();
    let st = AppState::new(&cfg, db, None).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(l, tokomotive::router(st)).await.unwrap() });
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
        .unwrap();
    let html = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The provider picker (no-JavaScript select) lists each sign-in type.
    for option in ["github-copilot", "qwen-code", "kimi-code"] {
        assert!(html.contains(&format!("value=\"{option}\"")), "{option}");
    }
    for (auth, base) in [
        ("github_copilot", "https://api.githubcopilot.com"),
        (
            "qwen_code",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ),
        ("kimi_code", "https://api.kimi.com/coding/v1"),
    ] {
        let r = c
            .post(format!("{url}/admin/providers"))
            .header("cookie", cookie)
            .form(&[
                ("name", auth),
                ("auth", auth),
                ("kind", "openai"),
                ("base_url", base),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303, "{auth}");
        let r = c
            .post(format!("{url}/admin/providers/{auth}/oauth/paste"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }
    let html = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("GitHub Enterprise domain"));
    assert_eq!(
        html.matches("Sign in with a device code</button>").count(),
        3
    );
    c.post(format!("{url}/admin/users"))
        .header("cookie", cookie)
        .form(&[("name", "operator")])
        .send()
        .await
        .unwrap();
    let html = c
        .get(format!("{url}/admin/keys"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for name in ["github_copilot", "qwen_code", "kimi_code"] {
        assert!(
            html.contains(&format!(
                "{name} <span class=\"tag info\">subscription</span>"
            )),
            "{name}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn subscription_proxy_guards_paths_identity_access_and_cost() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokomotive::{
        config::Config,
        db,
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    for (name, flow) in [
        ("github_copilot", Flow::GithubCopilot),
        ("qwen_code", Flow::QwenCode),
        ("kimi_code", Flow::KimiCode),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app=Router::new()
            .route("/device",post(||async{Json(json!({"device_code":"private","user_code":"PUBLIC","verification_uri":"https://issuer.example/activate","interval":1}))}))
            .route("/token",post(||async{Json(json!({"access_token":"subscription-private","refresh_token":"refresh","expires_in":3600}))}))
            .route("/user",get(||async{Json(json!({"chat_enabled":true}))}))
            .route(if flow==Flow::GithubCopilot{"/chat/completions"}else{"/v1/chat/completions"},post(move |h:axum::http::HeaderMap,Json(body):Json<serde_json::Value>|{let seen=seen.clone();async move{
                assert_eq!(h["authorization"],"Bearer subscription-private");assert_eq!(h["user-agent"],"Tokomotive/0.1.0");
                assert!(!h.contains_key("editor-version"));assert!(!h.contains_key("cookie"));
                match flow {
                    Flow::GithubCopilot=>{assert_eq!(h["x-github-api-version"],"2026-08-01");assert_eq!(h["openai-intent"],"conversation-edits");if body["messages"][0]["role"]=="tool" {assert_eq!(h["x-initiator"],"agent");assert_eq!(h["copilot-vision-request"],"true");} else {assert_eq!(h["x-initiator"],"user");assert!(!h.contains_key("copilot-vision-request"));} assert_eq!(h["x-interaction-type"],"conversation-agent");assert!(!h.contains_key("copilot-session-token"));},
                    Flow::QwenCode=>{assert_eq!(h["x-dashscope-authtype"],"qwen-oauth");assert_eq!(h["x-dashscope-useragent"],"Tokomotive/0.1.0");},
                    _=>{assert_eq!(h["x-msh-platform"],"tokomotive");assert_ne!(h["x-msh-device-id"],"spoof");assert!(h.contains_key("x-msh-version"));}
                }
                seen.fetch_add(1,Ordering::SeqCst);Json(json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1}}))
            }}));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let mock = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let resource = match flow {
            Flow::GithubCopilot => "https://api.githubcopilot.com",
            Flow::QwenCode => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            _ => "https://api.kimi.com/coding/v1",
        };
        let cfg:Config=toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='p'\nkind='openai'\nauth='{name}'\nbase_url='{resource}'\n[[models]]\nname='m'\nprovider='p'\ncost={{input=2,output=4}}\nprice={{input=3,output=5}}\n[[users]]\nname='allowed'\nkeys=['vk-allowed']\nkey_allowed_providers=['p']\n[[users]]\nname='denied'\nkeys=['vk-denied']")).unwrap();
        cfg.validate().unwrap();
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::seed(&db, &cfg).await.unwrap();
        let st = AppState::new_with_oauth_endpoints(
            &cfg,
            db,
            None,
            OAuthEndpoints {
                copilot_device: format!("{base}/device"),
                copilot_token: format!("{base}/token"),
                copilot_user: format!("{base}/user"),
                copilot_resource: base.clone(),
                qwen_device: format!("{base}/device"),
                qwen_token: format!("{base}/token"),
                qwen_resource: base.clone(),
                kimi_device: format!("{base}/device"),
                kimi_token: format!("{base}/token"),
                kimi_resource: base,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let a = st.oauth.start_device("p", flow).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1010)).await;
        st.oauth.poll_device("p", flow, &a.attempt).await.unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let app = tokomotive::router(st.clone());
        let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let c = reqwest::Client::new();
        for (path, key, expected) in [
            ("/v1/chat/completions", "vk-allowed", 200),
            ("/v1/chat/completions", "vk-denied", 403),
            ("/v1/organizations", "vk-allowed", 400),
            ("/v1/chat/completions/extra", "vk-allowed", 400),
            ("/v1/embeddings", "vk-allowed", 400),
        ] {
            let r = c
                .post(format!("{url}{path}"))
                .bearer_auth(key)
                .header("user-agent", "spoof")
                .header("editor-version", "spoof")
                .header("cookie", "spoof")
                .header("x-github-api-version", "spoof")
                .header("x-initiator", "spoof")
                .header("copilot-session-token", "spoof")
                .header("x-dashscope-authtype", "spoof")
                .header("x-dashscope-useragent", "spoof")
                .header("x-msh-platform", "spoof")
                .header("x-msh-device-id", "spoof")
                .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), expected, "{name} {path}");
            r.bytes().await.unwrap();
        }
        if flow == Flow::GithubCopilot {
            let r=c.post(format!("{url}/v1/chat/completions")).bearer_auth("vk-allowed").header("x-initiator","user").header("copilot-vision-request","false").json(&json!({"model":"m","messages":[{"role":"tool","tool_call_id":"call","content":"image follows"},{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]}]})).send().await.unwrap();
            assert_eq!(r.status(), 200);
            r.bytes().await.unwrap();
        }
        st.flush().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            if flow == Flow::GithubCopilot { 2 } else { 1 }
        );
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT cost_micros,price_micros,subscription FROM ledger WHERE status=200",
        )
        .fetch_one(&st.db)
        .await
        .unwrap();
        assert_eq!(row, (0, 11, 1));
        server.abort();
        mock.abort();
    }
}

#[tokio::test]
async fn unofficial_device_flows_poll_store_and_rotate() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        settings::SecretKey,
    };
    for (name, flow) in [
        ("github_copilot", Flow::GithubCopilot),
        ("qwen_code", Flow::QwenCode),
        ("kimi_code", Flow::KimiCode),
    ] {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let app=Router::new()
            .route("/device",post(move |headers:axum::http::HeaderMap,body:axum::body::Bytes|async move{
                let f:std::collections::HashMap<String,String>=if flow==Flow::GithubCopilot{serde_json::from_slice(&body).unwrap()}else{serde_urlencoded::from_bytes(&body).unwrap()};
                assert_eq!(f["client_id"],match flow{Flow::GithubCopilot=>"Ov23li8tweQw6odWQebz",Flow::QwenCode=>"f0304373b74a44d2b584a3fb70ca9e56",_=>"17e5f671-d194-4dfb-9706-5516cb48c098"});
                if flow==Flow::QwenCode{assert_eq!(f["scope"],"openid profile email model.completion");assert_eq!(f["code_challenge_method"],"S256");}
                if flow==Flow::KimiCode{assert_eq!(headers["x-msh-platform"],"tokomotive");assert_eq!(headers["x-msh-device-id"].len(),36);}
                Json(json!({"device_code":"private-device","user_code":"PUBLIC","verification_uri":"https://issuer.example/device","interval":1}))
            }))
            .route("/token",post(move |headers:axum::http::HeaderMap,body:axum::body::Bytes|{let seen=seen.clone();async move{
                let f:std::collections::HashMap<String,String>=if flow==Flow::GithubCopilot{serde_json::from_slice(&body).unwrap()}else{serde_urlencoded::from_bytes(&body).unwrap()};
                if flow==Flow::KimiCode{assert_eq!(headers["x-msh-platform"],"tokomotive");}
                if flow==Flow::QwenCode && f["grant_type"]!="refresh_token" {assert!(f["code_verifier"].len()>=43);}
                if f["grant_type"]=="refresh_token" {assert_eq!(f["refresh_token"],format!("private-refresh-{}",seen.load(Ordering::SeqCst)-1));}
                match seen.fetch_add(1,Ordering::SeqCst) {
                    0=>Json(json!({"error":"authorization_pending"})),
                    1=>Json(json!({"error":"slow_down"})),
                    n=>if flow==Flow::GithubCopilot {Json(json!({"access_token":format!("private-access-{n}")}))} else {Json(json!({"access_token":format!("private-access-{n}"),"refresh_token":format!("private-refresh-{n}"),"expires_in":3600,"resource_url":"portal.qwen.ai"}))}
                }
            }}))
            .route("/user",get(||async{Json(json!({"chat_enabled":true,"endpoints":{"api":"https://api.individual.githubcopilot.com"}}))}));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let db = db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('p','\"openai\"','https://unused',?)").bind(name).execute(&db).await.unwrap();
        let clock = Arc::new(AtomicI64::new(1000));
        let c = clock.clone();
        let endpoints = OAuthEndpoints {
            copilot_device: format!("{base}/device"),
            copilot_token: format!("{base}/token"),
            copilot_user: format!("{base}/user"),
            qwen_device: format!("{base}/device"),
            qwen_token: format!("{base}/token"),
            kimi_device: format!("{base}/device"),
            kimi_token: format!("{base}/token"),
            ..Default::default()
        };
        let auth = OAuthManager::new(db.clone(), SecretKey::from_bytes([7; 32]), endpoints)
            .unwrap()
            .with_clock(move || c.load(Ordering::SeqCst));
        let a = auth.start_device("p", flow).await.unwrap();
        assert_eq!(a.user_code, "PUBLIC");
        for now in [2000, 3000, 9000] {
            clock.store(now, Ordering::SeqCst);
            assert_eq!(
                auth.poll_device("p", flow, &a.attempt).await.unwrap(),
                now != 9000
            );
        }
        assert_eq!(
            auth.device_status("p", flow, &a.attempt)
                .await
                .unwrap()
                .state,
            "connected",
            "{name}"
        );
        assert_eq!(
            auth.token("p", flow, None).await.unwrap(),
            "private-access-2"
        );
        if flow == Flow::GithubCopilot {
            clock.store(100_000_000, Ordering::SeqCst);
            assert_eq!(
                auth.token("p", flow, None).await.unwrap(),
                "private-access-2"
            );
            assert!(
                auth.token("p", flow, Some("private-access-2"))
                    .await
                    .is_err()
            );
        }
        let resource = auth
            .inference_resource("p", serde_json::from_value(json!(name)).unwrap(), "unused")
            .await
            .unwrap();
        assert_eq!(
            resource,
            match flow {
                Flow::GithubCopilot => "https://api.individual.githubcopilot.com",
                Flow::QwenCode => "https://portal.qwen.ai/v1",
                _ => "https://api.kimi.com/coding/v1",
            }
        );
        if flow != Flow::GithubCopilot {
            assert_eq!(
                auth.token("p", flow, Some("private-access-2"))
                    .await
                    .unwrap(),
                "private-access-3"
            );
            assert_eq!(
                auth.token("p", flow, Some("private-access-3"))
                    .await
                    .unwrap(),
                "private-access-4"
            );
        }
        let secret: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!secret.contains("private-"));
        if flow == Flow::KimiCode {
            let id = auth.device_id("p", flow).await.unwrap();
            assert!(!secret.contains(&id));
            let restarted = OAuthManager::new(
                db.clone(),
                SecretKey::from_bytes([7; 32]),
                OAuthEndpoints::default(),
            )
            .unwrap();
            assert_eq!(restarted.device_id("p", flow).await.unwrap(), id);
            auth.disconnect("p", flow).await.unwrap();
            auth.start_device("p", flow).await.unwrap();
            assert_eq!(auth.device_id("p", flow).await.unwrap(), id);
        }
        server.abort();
    }
}
