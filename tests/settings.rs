use tokomotive::settings::SecretKey;
use tokomotive::{config::Config, state::AppState};

#[tokio::test]
async fn master_key_is_next_to_database_with_encoded_parent_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/with%20space/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    assert!(dir.path().join("with space/secret.key").exists());
    st.db.close().await;
}

async fn app() -> (
    tempfile::TempDir,
    AppState,
    String,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, st, url, task)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn bedrock_credentials_are_encrypted_not_rendered_and_region_is_validated() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let fields = [
        ("name", "aws"),
        ("kind", "bedrock"),
        ("base_url", "https://ignored.example"),
        ("aws_region", "us-east-1"),
        ("aws_access_key", "PRIVATEACCESSKEY"),
        ("aws_secret_key", "PRIVATESECRETKEY"),
        ("aws_session_token", "PRIVATESESSIONTOKEN"),
    ];
    let r = c
        .post(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .form(&fields)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let creds = tokomotive::settings::bedrock_credentials(&st.db, &st.secret_key, "aws")
        .await
        .unwrap();
    assert_eq!(creds.secret_key, "PRIVATESECRETKEY");
    let raw: String =
        sqlx::query_scalar("SELECT bedrock_credentials FROM providers WHERE name='aws'")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(!raw.contains("PRIVATE"));
    let page = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("PRIVATE"));
    assert!(page.contains("AWS credentials saved"));
    assert!(page.contains("type=\"password\" name=\"aws_secret_key\""));
    for region in ["us-east-1.evil", "us-east-1/path", "us-east-1\r\nx: y"] {
        let mut bad = fields;
        bad[0].1 = "bad";
        bad[3].1 = region;
        assert_eq!(
            c.post(format!("{url}/admin/providers"))
                .header("cookie", &cookie)
                .form(&bad)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    task.abort();
}

#[tokio::test]
async fn code_assist_preset_uses_paste_panel_without_publishing_secret() {
    let (_dir, _st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let page = c
        .get(format!("{url}/admin/providers?preset=gemini-cli"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("value=\"gemini_cli\" selected"));
    assert_eq!(
        c.post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", "cli"),
                ("auth", "gemini_cli"),
                ("base_url", "https://cloudcode-pa.googleapis.com")
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let page = c
        .post(format!("{url}/admin/providers/cli/oauth/start"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("codeassist.google.com/authcode"));
    assert!(page.contains("Code Assist"));
    assert!(!page.contains("GOCSPX"));
    assert!(!page.contains("device code"));
    task.abort();
}

#[tokio::test]
async fn vertex_forms_validate_and_store_region_project_without_pending_state() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    for (project, region, status) in [
        ("test-project", "us-central1", 303),
        ("../bad-project", "global", 400),
        ("test-project", "global.evil", 400),
    ] {
        let r = c
            .post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", "vertex"),
                ("auth", "google_vertex"),
                ("base_url", "https://ignored.example"),
                ("project_id", project),
                ("region", region),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status);
    }
    let page = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("adapter pending"));
    assert!(page.contains("name=\"project_id\" value=\"test-project\""));
    assert!(page.contains("name=\"region\" value=\"us-central1\""));
    assert!(page.contains("Service-account JSON key"));
    let cfg = tokomotive::settings::load(&st.db, &st.secret_key)
        .await
        .unwrap()
        .0;
    assert_eq!(
        cfg.providers[0].base_url,
        "https://us-central1-aiplatform.googleapis.com"
    );
    let r = c
        .post(format!(
            "{url}/admin/providers/vertex/oauth/service-account"
        ))
        .header("cookie", &cookie)
        .form(&[("service_account", "not json")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let choices = tokomotive::db::search_names(&st.db, "providers", "vertex")
        .await
        .unwrap();
    assert_eq!(choices.len(), 1);
    task.abort();
}

#[tokio::test]
async fn preset_fills_editable_provider_fields_without_javascript() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let page = c
        .get(format!("{url}/admin/providers?preset=zai-coding"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("name=\"preset\""));
    assert!(page.contains("value=\"zai-coding\" required"));
    assert!(page.contains("value=\"https://api.z.ai/api/coding/paas/v4\""));
    assert!(page.contains("https://docs.z.ai/devpack/quick-start"));
    assert!(page.contains("name=\"include_usage\""));
    assert!(page.contains("name=\"key_header\""));
    assert_eq!(
        c.post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", "custom"),
                ("kind", "anthropic"),
                ("base_url", "https://custom.example/anthropic"),
                ("key_header", "bearer"),
                ("include_usage", "false")
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    st.reload_settings().await.unwrap();
    let cfg = tokomotive::settings::load(&st.db, &st.secret_key)
        .await
        .unwrap()
        .0;
    let serialized = serde_json::to_value(&cfg.providers[0]).unwrap();
    assert_eq!(serialized["key_header"], "bearer");
    assert_eq!(serialized["include_usage"], false);
    task.abort();
}

#[tokio::test]
async fn gemini_preset_forms_model_list_and_test_use_native_api() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let page = c
        .get(format!("{url}/admin/providers?preset=gemini"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("value=\"gemini\" selected>Gemini"));
    assert!(page.contains("value=\"x-goog-api-key\" selected"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let upstream = axum::Router::new().route("/v1beta/models",axum::routing::get(|headers:axum::http::HeaderMap|async move {
        assert_eq!(headers["x-goog-api-key"],"private-gemini-key");
        axum::Json(serde_json::json!({"models":[{"name":"models/gemini-2.5-flash","displayName":"Gemini Flash","inputTokenLimit":1048576}]}))
    }));
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let r = c
        .post(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "gemini"),
            ("kind", "gemini"),
            ("base_url", &base),
            ("api_key", "private-gemini-key"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let page = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("private-gemini-key"));
    assert!(page.contains("value=\"gemini\" selected>Gemini"));
    let test = c
        .post(format!("{url}/admin/providers/gemini/test"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(test.contains("OK"), "{test}");
    let list: serde_json::Value = c
        .get(format!(
            "{url}/admin/search/models?provider=gemini&src=provider&q=gemini"
        ))
        .header("cookie", &cookie)
        .header("accept", "application/json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["id"], "models/gemini-2.5-flash");
    assert_eq!(list[0]["label"], "Gemini Flash");
    assert_eq!(list[0]["context"], 1048576);
    st.db.close().await;
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn provider_key_headers_replace_all_client_credentials() {
    use std::sync::{Arc, Mutex};
    let (_dir, st, url, task) = app().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let upstream =
        axum::Router::new().fallback(move |uri: axum::http::Uri, mut h: axum::http::HeaderMap| {
            h.insert("x-seen-path", uri.path().parse().unwrap());
            capture.lock().unwrap().push(h);
            async { axum::Json(serde_json::json!({"usage":{"input_tokens":2,"output_tokens":1}})) }
        });
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let user = tokomotive::db::create_user(&st.db, "header-user", None, None, false)
        .await
        .unwrap();
    let (_, key) = tokomotive::db::create_key(&st.db, user, "headers", None)
        .await
        .unwrap();
    for (name, kind, header, path, prefix, expected) in [
        (
            "kimi",
            "anthropic",
            "bearer",
            "messages",
            "/anthropic",
            "/anthropic/v1/messages",
        ),
        (
            "azure",
            "openai",
            "api-key",
            "chat/completions",
            "/openai/v1",
            "/openai/v1/chat/completions",
        ),
        (
            "anthropic",
            "anthropic",
            "x-api-key",
            "messages",
            "",
            "/v1/messages",
        ),
        (
            "router",
            "anthropic",
            "bearer",
            "messages",
            "/api/v1/messages",
            "/api/v1/messages",
        ),
        (
            "zai",
            "openai",
            "bearer",
            "chat/completions",
            "/api/paas/v4",
            "/api/paas/v4/chat/completions",
        ),
        (
            "deepinfra",
            "openai",
            "bearer",
            "chat/completions",
            "/v1/openai",
            "/v1/openai/chat/completions",
        ),
    ] {
        let provider_base = format!("{base}{prefix}");
        assert_eq!(
            c.post(format!("{url}/admin/providers"))
                .header("cookie", &cookie)
                .form(&[
                    ("name", name),
                    ("kind", kind),
                    ("key_header", header),
                    ("base_url", &provider_base),
                    ("api_key", "upstream-secret")
                ])
                .send()
                .await
                .unwrap()
                .status(),
            303
        );
        let response = c
            .post(format!("{url}/v1/{path}"))
            .bearer_auth(&key)
            .header("x-api-key", "client-x-key")
            .header("api-key", "client-azure-key")
            .json(&serde_json::json!({"model":format!("{name}/demo"),"messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let _ = response.bytes().await.unwrap();
        let h = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(h["x-seen-path"], expected);
        let selected = if header == "bearer" {
            "authorization"
        } else {
            header
        };
        assert_eq!(
            h.get(selected).unwrap().to_str().unwrap(),
            if header == "bearer" {
                "Bearer upstream-secret"
            } else {
                "upstream-secret"
            }
        );
        for other in ["authorization", "x-api-key", "api-key"] {
            if other != selected {
                assert!(!h.contains_key(other), "forwarded {other}");
            }
        }
        if kind == "anthropic" {
            assert_eq!(h["anthropic-version"], "2023-06-01");
        }
        // Test connection must use the same key-header policy.
        c.post(format!("{url}/admin/providers/{name}/test"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().last().unwrap().get(selected),
            h.get(selected)
        );
    }
    task.abort();
    upstream_task.abort();
}

async fn authenticate(c: &reqwest::Client, url: &str) -> String {
    c.post(format!("{url}/setup"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    c.post(format!("{url}/admin/login"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap()
        .headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}

#[tokio::test]
async fn setup_mode_choice_is_clear_validated_and_persists() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let html = c
        .get(format!("{url}/setup"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("name=\"mode\""));
    assert!(html.contains("value=\"full\" checked"));
    assert!(html.contains("value=\"simple\""));
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "long-password"), ("mode", "wrong")])
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "long-password"), ("mode", "simple")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(st.settings().mode, "simple");
    st.reload_settings().await.unwrap();
    assert_eq!(st.settings().mode, "simple");
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "another-password"), ("mode", "full")])
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(st.settings().mode, "simple");
    task.abort();
}

#[tokio::test]
async fn simple_seed_and_hidden_settings_survive_edits_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str(
        "[server]\nmode = 'simple'\ndefault_markup_percent = 37\nreserve_output_tokens = 123",
    )
    .unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    assert_eq!(st.settings().mode, "simple");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = client();
    let cookie = authenticate(&c, &url).await;
    assert_eq!(st.settings().mode, "simple"); // Legacy password-only Setup preserves the seed.
    let user = tokomotive::db::create_user_with_limits(
        &st.db,
        "limited",
        None,
        Some(1234),
        true,
        tokomotive::ratelimit::Limits {
            rpm: Some(5),
            tpm: Some(6),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        c.post(format!("{url}/admin/users/{user}"))
            .header("cookie", &cookie)
            .form(&[("disabled", "on")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let users = tokomotive::db::list_users(&st.db).await.unwrap();
    assert_eq!(
        (
            users[0].budget_micros,
            users[0].budget_monthly,
            users[0].rpm_limit,
            users[0].tpm_limit
        ),
        (Some(1234), true, Some(5), Some(6))
    );
    assert!(users[0].disabled);
    assert_eq!(
        c.post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", "p"),
                ("kind", "openai"),
                ("base_url", "https://example.com")
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        c.post(format!("{url}/admin/models"))
            .header("cookie", &cookie)
            .form(&[("name", "demo"), ("provider", "p"), ("aliases", "alias")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let cat = st.catalog();
    let model = cat.models.iter().find(|m| m.name == "demo").unwrap();
    assert!(model.cost.is_none());
    assert_eq!(model.aliases, ["alias"]);
    assert_eq!(
        c.post(format!("{url}/admin/settings"))
            .header("cookie", &cookie)
            .form(&[
                ("upstream_timeout_secs", "45"),
                ("session_days", "7"),
                ("failure_threshold", "3"),
                ("cooldown_secs", "30"),
                ("max_attempts", "8"),
                ("mode", "full")
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(st.settings().reserve_output_tokens, 123);
    assert_eq!(st.catalog().default_markup_bp, 3700);
    // A later seed cannot undo the saved mode.
    let restarted = tokomotive::init(&cfg, None).await.unwrap();
    assert_eq!(restarted.settings().mode, "full");
    assert_eq!(restarted.settings().upstream_timeout_secs, 45);
    task.abort();
}

#[tokio::test]
async fn provider_created_in_dashboard_is_live_and_keys_are_redacted() {
    let (_dir, st, url, task) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|h: axum::http::HeaderMap| async move {
        assert!(matches!(h["authorization"].to_str().unwrap(), "Bearer secret-value-1234" | "Bearer replacement-key-5678"));
        axum::Json(serde_json::json!({"model":"demo","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
    })).route("/v1/models", axum::routing::get(|h: axum::http::HeaderMap| async move {
        assert_eq!(h["authorization"], "Bearer replacement-key-5678");
        axum::http::StatusCode::OK
    }));
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let r = c
        .post(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "test"),
            ("kind", "openai"),
            ("base_url", upstream_url.as_str()),
            ("api_key", "secret-value-1234"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let html = c
        .get(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("secret-value"));
    assert!(html.contains("1234"));
    let stored: String = sqlx::query_scalar("SELECT keys_json FROM providers WHERE name='test'")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert!(!stored.contains("secret-value"));
    let user = tokomotive::db::create_user(&st.db, "demo", None, None, false)
        .await
        .unwrap();
    let (_, key) = tokomotive::db::create_key(&st.db, user, "demo", None)
        .await
        .unwrap();
    let r = c
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&serde_json::json!({"model":"test/demo","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(
        c.post(format!("{url}/admin/providers/test/keys"))
            .header("cookie", &cookie)
            .form(&[("api_key", "replacement-key-5678")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        c.post(format!("{url}/admin/providers/test/keys/0/delete"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let r = c
        .post(format!("{url}/admin/providers/test/test"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("The Provider answered 200")
    );
    assert_eq!(
        c.post(format!("{url}/v1/chat/completions"))
            .bearer_auth(&key)
            .json(&serde_json::json!({"model":"test/demo","messages":[]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn route_edits_apply_live_and_invalid_changes_are_refused() {
    let (_dir, st, url, task) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|axum::Json(b): axum::Json<serde_json::Value>| async move {
        axum::Json(serde_json::json!({"model":b["model"],"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
    }));
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let c = client();
    let cookie = authenticate(&c, &url).await;
    assert_eq!(
        c.post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", "p"),
                ("kind", "openai"),
                ("base_url", &upstream_url)
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let route = |model: &str, weight: &str| {
        [
            ("deployment_provider", "p".to_string()),
            ("deployment_model", model.to_string()),
            ("deployment_weight", weight.to_string()),
        ]
    };
    let r = c
        .post(format!("{url}/admin/routes"))
        .header("cookie", &cookie)
        .form(
            &[
                ("name", "smart".to_string()),
                ("strategy", "priority".to_string()),
                ("fallbacks", String::new()),
            ]
            .into_iter()
            .chain(route("first", "1"))
            .collect::<Vec<_>>(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let user = tokomotive::db::create_user(&st.db, "demo", None, None, false)
        .await
        .unwrap();
    let (_, key) = tokomotive::db::create_key(&st.db, user, "demo", None)
        .await
        .unwrap();
    let request = || {
        c.post(format!("{url}/v1/chat/completions"))
            .bearer_auth(&key)
            .json(&serde_json::json!({"model":"smart","messages":[]}))
    };
    assert_eq!(
        request()
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["model"],
        "first"
    );
    assert_eq!(
        c.post(format!("{url}/admin/routes/smart"))
            .header("cookie", &cookie)
            .form(
                &[
                    ("name", "smart".to_string()),
                    ("strategy", "weighted".to_string()),
                ]
                .into_iter()
                .chain(route("second", "1"))
                .collect::<Vec<_>>()
            )
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        request()
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["model"],
        "second"
    );
    let invalid = c
        .post(format!("{url}/admin/routes/smart"))
        .header("cookie", &cookie)
        .form(
            &[
                ("name", "smart".to_string()),
                ("strategy", "priority".to_string()),
            ]
            .into_iter()
            .chain(route("broken", "0"))
            .collect::<Vec<_>>(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(
        invalid
            .text()
            .await
            .unwrap()
            .contains("zero deployment weight")
    );
    let in_use = c
        .post(format!("{url}/admin/providers/p/delete"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(in_use.status(), 400);
    assert!(
        in_use
            .text()
            .await
            .unwrap()
            .contains("A Route or a Model uses this Provider")
    );
    assert_eq!(
        request()
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["model"],
        "second"
    );
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn route_row_editor_creates_validates_and_prefills_deployments() {
    let (_dir, st, url, task) = app().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|axum::Json(b): axum::Json<serde_json::Value>| async move {
        axum::Json(serde_json::json!({"model":b["model"],"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
    }));
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let c = client();
    let cookie = authenticate(&c, &url).await;
    for name in ["alpha", "beta"] {
        let r = c
            .post(format!("{url}/admin/providers"))
            .header("cookie", &cookie)
            .form(&[
                ("name", name),
                ("kind", "openai"),
                ("base_url", &upstream_url),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303);
    }
    let post = |path: &str, fields: Vec<(&'static str, &'static str)>| {
        c.post(format!("{url}{path}"))
            .header("cookie", &cookie)
            .form(&fields)
            .send()
    };
    // Two deployments, one blank row that the server ignores.
    let r = post(
        "/admin/routes",
        vec![
            ("name", "rows"),
            ("strategy", "weighted"),
            ("fallbacks", ""),
            ("deployment_provider", "beta"),
            ("deployment_model", "upstream-one"),
            ("deployment_weight", "3"),
            ("deployment_provider", ""),
            ("deployment_model", "  "),
            ("deployment_weight", "1"),
            ("deployment_provider", "alpha"),
            ("deployment_model", "upstream-two"),
            ("deployment_weight", "7"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let route = st
        .router_table()
        .routes
        .iter()
        .find(|r| r.name == "rows")
        .cloned()
        .expect("route is live");
    let got: Vec<_> = route
        .deployments
        .iter()
        .map(|d| (d.provider.clone(), d.model.clone(), d.weight))
        .collect();
    assert_eq!(
        got,
        vec![
            ("beta".to_string(), "upstream-one".to_string(), 3),
            ("alpha".to_string(), "upstream-two".to_string(), 7)
        ]
    );
    let user = tokomotive::db::create_user(&st.db, "rows", None, None, false)
        .await
        .unwrap();
    let (_, key) = tokomotive::db::create_key(&st.db, user, "rows", None)
        .await
        .unwrap();
    let answer: serde_json::Value = c
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&serde_json::json!({"model":"rows","messages":[]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(
            answer["model"].as_str(),
            Some("upstream-one" | "upstream-two")
        ),
        "{answer}"
    );

    for (fields, message) in [
        (
            vec![
                ("deployment_provider", "missing"),
                ("deployment_model", "m"),
                ("deployment_weight", "1"),
            ],
            "unknown Provider missing",
        ),
        (
            vec![
                ("deployment_provider", "alpha"),
                ("deployment_model", "m"),
                ("deployment_weight", "0"),
            ],
            "zero deployment weight",
        ),
        (
            vec![
                ("deployment_provider", "alpha"),
                ("deployment_model", "m"),
                ("deployment_weight", "-2"),
            ],
            "Weight must be a positive integer",
        ),
        (
            vec![
                ("deployment_provider", ""),
                ("deployment_model", ""),
                ("deployment_weight", "1"),
            ],
            "Add at least one Deployment",
        ),
        (vec![], "Add at least one Deployment"),
    ] {
        let mut all = vec![("name", "rows"), ("strategy", "priority")];
        all.extend(fields);
        let r = post("/admin/routes/rows", all).await.unwrap();
        assert_eq!(r.status(), 400);
        let body = r.text().await.unwrap();
        assert!(body.contains("role=\"alert\""), "{body}");
        assert!(body.contains(message), "{message}: {body}");
    }
    assert_eq!(
        st.router_table()
            .routes
            .iter()
            .find(|r| r.name == "rows")
            .unwrap()
            .deployments
            .len(),
        2,
        "refused edits leave the route unchanged"
    );

    let page = c
        .get(format!("{url}/admin/routes"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("Deployment JSON"), "JSON textarea is gone");
    let edit = page
        .split("action=\"/admin/routes/rows\"")
        .nth(1)
        .and_then(|s| s.split("</form>").next())
        .expect("edit form for the route");
    // Without JavaScript, a spare blank row inside <noscript> lets the operator add one.
    let (edit, spare) = edit.split_once("<noscript>").expect("spare row for no-JS");
    assert!(spare.contains("name=\"deployment_model\" value=\"\""));
    assert_eq!(edit.matches("name=\"deployment_provider\"").count(), 2);
    assert_eq!(edit.matches("name=\"deployment_model\"").count(), 2);
    assert!(
        edit.contains("<option value=\"beta\" selected>beta</option>"),
        "{edit}"
    );
    assert!(edit.contains("value=\"upstream-one\""));
    assert!(edit.contains("value=\"3\""));
    assert!(edit.contains("value=\"upstream-two\""));
    assert!(edit.contains("value=\"7\""));
    assert!(
        edit.find("upstream-one") < edit.find("upstream-two"),
        "order kept"
    );
    assert!(page.contains("data-picker=\"multi\" data-ordered data-src=\"route,model\"><input id=\"nr-fb\" name=\"fallbacks\" value=\"\""));
    task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn server_settings_and_password_changes_are_live_and_cross_origin_posts_fail() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let cookie = authenticate(&c, &url).await;
    let form = [
        ("default_markup_percent", "15"),
        ("upstream_timeout_secs", "22"),
        ("reserve_output_tokens", "123"),
        ("session_days", "2"),
        ("failure_threshold", "2"),
        ("cooldown_secs", "10"),
        ("max_attempts", "3"),
        ("mode", "simple"),
        ("metrics_token", "metrics-secret-4321"),
    ];
    let r = c
        .post(format!("{url}/admin/settings"))
        .header("cookie", &cookie)
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    assert_eq!(st.settings().mode, "simple");
    assert_eq!(st.settings().reserve_output_tokens, 123);
    assert_eq!(st.settings().upstream_timeout_secs, 22);
    assert_eq!(st.router_table().settings.max_attempts, 3);
    assert_eq!(st.catalog().default_markup_bp, 1500);
    assert_eq!(
        c.get(format!("{url}/metrics"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.get(format!("{url}/metrics"))
            .bearer_auth("metrics-secret-4321")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let html = c
        .get(format!("{url}/admin/settings"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("metrics-secret"));
    let stored: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='runtime_settings'")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(!stored.contains("metrics-secret"));
    assert_eq!(
        c.post(format!("{url}/admin/settings"))
            .header("cookie", &cookie)
            .header("origin", "https://evil.example")
            .form(&form)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.post(format!("{url}/admin/settings/password"))
            .header("cookie", &cookie)
            .form(&[("current_password", "wrong"), ("password", "new-password")])
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{url}/admin/settings/password"))
            .header("cookie", &cookie)
            .form(&[
                ("current_password", "long-password"),
                ("password", "new-password")
            ])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        c.get(format!("{url}/admin/settings"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        c.post(format!("{url}/admin/login"))
            .form(&[("password", "new-password")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    task.abort();
}

#[tokio::test]
async fn setup_password_once_and_login() {
    let (_dir, st, url, task) = app().await;
    let c = client();
    let r = c.get(format!("{url}/admin")).send().await.unwrap();
    assert_eq!(r.headers()["location"], "/setup");
    assert!(
        c.get(format!("{url}/setup"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("Set up Tokomotive")
    );
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "short")])
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "long-password")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        c.post(format!("{url}/setup"))
            .form(&[("password", "other-password")])
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let stored: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key='admin_password_hash'")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert!(stored.starts_with("$argon2"));
    assert!(!stored.contains("long-password"));
    let login = c
        .post(format!("{url}/admin/login"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    assert!(login.headers().contains_key("set-cookie"));
    task.abort();
}

#[test]
fn secrets_round_trip_and_wrong_master_key_fails() {
    let key = SecretKey::from_bytes([7; 32]);
    let value = key.encrypt("provider-secret").unwrap();
    assert!(!value.contains("provider-secret"));
    assert_eq!(key.decrypt(&value).unwrap(), "provider-secret");
    assert!(SecretKey::from_bytes([8; 32]).decrypt(&value).is_err());
    assert_ne!(value, key.encrypt("provider-secret").unwrap());
}

#[tokio::test]
async fn binary_starts_without_a_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tokomotive"))
        .current_dir(dir.path())
        .args([
            "--listen",
            &addr.to_string(),
            "--database",
            "sqlite://nested/db.sqlite",
        ])
        .env_remove("TOKOMOTIVE_CONFIG")
        .spawn()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        if client()
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "binary must start with no config.toml");
    assert!(dir.path().join("nested/secret.key").exists());
}

#[tokio::test]
async fn runtime_seed_is_once_and_wrong_key_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str(
        r#"
[[providers]]
name = "seeded"
kind = "openai"
base_url = "https://first.example"
api_key = "seed-secret"
api_keys_env = ["TOKOMOTIVE_SETTINGS_MISSING_TEST_KEY"]
key_header = "api-key"
include_usage = false
[[routes]]
name = "r"
deployments = [{provider="seeded",model="first",weight=1}]
"#,
    )
    .unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let first = tokomotive::init(&cfg, None).await.unwrap();
    assert_eq!(first.router_table().providers[0].keys[0], "seed-secret");
    assert_eq!(
        first.router_table().providers[0].key_header,
        tokomotive::provider::KeyHeader::ApiKey
    );
    assert!(!first.router_table().providers[0].include_usage);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("secret.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let stored: String = sqlx::query_scalar("SELECT keys_json FROM providers")
        .fetch_one(&first.db)
        .await
        .unwrap();
    assert!(stored.contains("env:TOKOMOTIVE_SETTINGS_MISSING_TEST_KEY"));
    assert!(!stored.contains("seed-secret"));
    cfg.providers[0].base_url = "https://later.example".into();
    cfg.router.max_attempts = 99;
    cfg.routes[0].deployments[0].model = "later".into();
    let second = tokomotive::init(&cfg, None).await.unwrap();
    assert_eq!(
        second.router_table().providers[0].base_url,
        "https://first.example"
    );
    assert_eq!(
        second.router_table().routes[0].deployments[0].model,
        "first"
    );
    assert_eq!(second.router_table().settings.max_attempts, 8);
    use base64::Engine;
    std::fs::write(
        dir.path().join("secret.key"),
        base64::engine::general_purpose::STANDARD.encode([8; 32]),
    )
    .unwrap();
    let error = match tokomotive::init(&cfg, None).await {
        Ok(_) => panic!("wrong master key must stop startup"),
        Err(e) => e,
    };
    assert!(error.to_string().contains("restore the original"));
}
