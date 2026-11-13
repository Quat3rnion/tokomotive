use llm_accounting::settings::SecretKey;
use llm_accounting::{config::Config, state::AppState};

#[tokio::test]
async fn master_key_is_next_to_database_with_encoded_parent_path() {
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/with%20space/db.sqlite", dir.path().display());
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    assert!(dir.path().join("with space/secret.key").exists());
    st.db.close().await;
}

async fn app() -> (
    tempfile::TempDir,
    AppState,
    String,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = llm_accounting::router(st.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, st, url, task)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
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
    let user = llm_accounting::db::create_user(&st.db, "demo", None, None, false)
        .await
        .unwrap();
    let (_, key) = llm_accounting::db::create_key(&st.db, user, "demo", None)
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
    assert!(r.text().await.unwrap().contains("Upstream status: 200"));
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
    let route = |model: &str, weight: u32| {
        serde_json::json!([{"provider":"p","model":model,"weight":weight}]).to_string()
    };
    let r = c
        .post(format!("{url}/admin/routes"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "smart"),
            ("strategy", "priority"),
            ("deployments", &route("first", 1)),
            ("fallbacks", ""),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let user = llm_accounting::db::create_user(&st.db, "demo", None, None, false)
        .await
        .unwrap();
    let (_, key) = llm_accounting::db::create_key(&st.db, user, "demo", None)
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
            .form(&[
                ("name", "smart"),
                ("strategy", "weighted"),
                ("deployments", &route("second", 1))
            ])
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
        .form(&[
            ("name", "smart"),
            ("strategy", "priority"),
            ("deployments", &route("broken", 0)),
        ])
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
    assert!(in_use.text().await.unwrap().contains("used by a route"));
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
            .contains("Set admin password")
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
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_llm-accounting"))
        .current_dir(dir.path())
        .args([
            "--listen",
            &addr.to_string(),
            "--database",
            "sqlite://nested/db.sqlite",
        ])
        .env_remove("LLM_ACCOUNTING_CONFIG")
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
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let mut cfg: Config = toml::from_str(
        r#"
[[providers]]
name = "seeded"
kind = "openai"
base_url = "https://first.example"
api_key = "seed-secret"
api_keys_env = ["LLMA_SETTINGS_MISSING_TEST_KEY"]
[[routes]]
name = "r"
deployments = [{provider="seeded",model="first",weight=1}]
"#,
    )
    .unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let first = llm_accounting::init(&cfg, None).await.unwrap();
    assert_eq!(first.router_table().providers[0].keys[0], "seed-secret");
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
    assert!(stored.contains("env:LLMA_SETTINGS_MISSING_TEST_KEY"));
    assert!(!stored.contains("seed-secret"));
    cfg.providers[0].base_url = "https://later.example".into();
    cfg.router.max_attempts = 99;
    cfg.routes[0].deployments[0].model = "later".into();
    let second = llm_accounting::init(&cfg, None).await.unwrap();
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
    let error = match llm_accounting::init(&cfg, None).await {
        Ok(_) => panic!("wrong master key must stop startup"),
        Err(e) => e,
    };
    assert!(error.to_string().contains("restore the original"));
}
