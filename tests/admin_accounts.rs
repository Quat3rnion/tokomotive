use std::future::IntoFuture;
use tokomotive::{config::Config, state::AppState};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for App {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn app() -> App {
    app_with_endpoints(Default::default()).await
}
async fn app_with_endpoints(endpoints: tokomotive::oauth::OAuthEndpoints) -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let db = tokomotive::db::connect(&cfg.server.database).await.unwrap();
    tokomotive::db::seed(&db, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(&cfg, db, None, endpoints)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    App {
        _dir: dir,
        st,
        url,
        c: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        task,
    }
}

#[tokio::test]
async fn refresh_now_works_for_admin_rejects_viewer_and_cross_origin_and_hides_secrets() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokomotive::oauth::{Flow, OAuthEndpoints};
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(l, Router::new().route("/token", post(move || {let seen = seen.clone(); async move {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            Json(json!({"access_token":format!("private-access-{n}"),"refresh_token":format!("private-refresh-{n}"),"expires_in":3600}))
        }}))).await.unwrap();
    });
    let a = app_with_endpoints(OAuthEndpoints {
        claude_token: format!("{base}/token"),
        claude_resource: base,
        ..Default::default()
    })
    .await;
    let owner = a.setup().await;
    add(&a, &owner, "operator", "admin").await;
    add(&a, &owner, "reader", "viewer").await;
    let admin = a.login("operator", "account-password").await;
    let viewer = a.login("reader", "account-password").await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &admin,
            &[
                ("name", "sub"),
                ("kind", "anthropic"),
                ("base_url", "https://api.anthropic.com"),
                ("auth", "claude")
            ]
        )
        .await
        .status(),
        303
    );
    let start = a.st.oauth.start("sub", Flow::Claude).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&start.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    a.st.oauth
        .finish("sub", Flow::Claude, &format!("code#{}", q["state"]))
        .await
        .unwrap();
    let path = "/admin/providers/sub/oauth/refresh";
    assert_eq!(a.post(path, &viewer, &[]).await.status(), 403);
    assert_eq!(
        a.c.post(format!("{}{path}", a.url))
            .header("cookie", &admin)
            .header("origin", "https://other.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(a.post(path, &admin, &[]).await.status(), 303);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let html =
        a.c.get(format!("{}/admin/providers", a.url))
            .header("cookie", &admin)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    for text in [
        "Access expires",
        "Last refresh",
        "Next refresh",
        "Refresh now",
    ] {
        assert!(html.contains(text), "{text}");
    }
    for secret in ["private-access", "private-refresh"] {
        assert!(!html.contains(secret));
    }
    let html =
        a.c.get(format!("{}/admin/providers", a.url))
            .header("cookie", &viewer)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(!html.contains("Refresh now"));
    mock.abort();
}
fn cookie(r: &reqwest::Response) -> String {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|h| h.to_str().ok())
        .find(|h| h.starts_with("tokomotive_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
impl App {
    async fn post(&self, path: &str, session: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", session)
            .form(form)
            .send()
            .await
            .unwrap()
    }
    async fn setup(&self) -> String {
        let r = self
            .post(
                "/setup",
                "",
                &[("username", "chief"), ("password", "owner-password")],
            )
            .await;
        assert_eq!(r.status(), 303);
        cookie(&r)
    }
    async fn login(&self, username: &str, password: &str) -> String {
        let r = self
            .post(
                "/admin/login",
                "",
                &[("username", username), ("password", password)],
            )
            .await;
        assert_eq!(r.status(), 303);
        cookie(&r)
    }
}

#[tokio::test]
async fn provider_env_references_require_owner_and_reject_server_secrets() {
    let a = app().await;
    let owner = a.setup().await;
    add(&a, &owner, "operator", "admin").await;
    let admin = a.login("operator", "account-password").await;
    let form = [
        ("name", "env-provider"),
        ("kind", "openai"),
        ("base_url", "https://provider.example"),
        ("api_key", "env:OPENAI_API_KEY"),
    ];
    assert_eq!(
        a.post("/admin/providers", &admin, &form).await.status(),
        403
    );
    for name in [
        "TOKOMOTIVE_ADMIN_PASSWORD",
        "TOKOMOTIVE_SECRET_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "DATABASE_URL",
    ] {
        let mut denied = form;
        let reference = format!("env:{name}");
        denied[3].1 = &reference;
        assert_eq!(
            a.post("/admin/providers", &owner, &denied).await.status(),
            400,
            "{name}"
        );
    }
    assert_eq!(
        a.post("/admin/providers", &owner, &form).await.status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/providers/env-provider/keys",
            &admin,
            &[("api_key", "env:ANTHROPIC_API_KEY")]
        )
        .await
        .status(),
        403
    );
    let mut redirected = form;
    redirected[2].1 = "https://other.example";
    redirected[3].1 = "";
    assert_eq!(
        a.post("/admin/providers/env-provider", &admin, &redirected)
            .await
            .status(),
        403
    );
    redirected[2].1 = "https://provider.example";
    let mut header = redirected.to_vec();
    header.push(("key_header", "api-key"));
    assert_eq!(
        a.post("/admin/providers/env-provider", &admin, &header)
            .await
            .status(),
        403
    );
    assert_eq!(
        a.post("/admin/providers/env-provider/keys/0/delete", &admin, &[])
            .await
            .status(),
        403
    );
    assert_eq!(
        a.post(
            "/admin/providers/env-provider/keys",
            &admin,
            &[("api_key", "pasted-provider-key")]
        )
        .await
        .status(),
        303
    );
    sqlx::query("UPDATE providers SET keys_json='[\"env:TOKOMOTIVE_ADMIN_PASSWORD\"]' WHERE name='env-provider'")
        .execute(&a.st.db).await.unwrap();
    assert!(
        tokomotive::settings::load(&a.st.db, &a.st.secret_key)
            .await
            .is_err(),
        "Old data must not resolve a server secret."
    );
}

#[tokio::test]
async fn audit_retention_is_owner_only_in_authorization_and_forms() {
    let a = app().await;
    let owner = a.setup().await;
    add(&a, &owner, "operator", "admin").await;
    let admin = a.login("operator", "account-password").await;
    assert_eq!(
        a.post("/admin/settings/audit", &admin, &[("days", "1")])
            .await
            .status(),
        403
    );
    for (session, allowed) in [(&owner, true), (&admin, false)] {
        let html =
            a.c.get(format!("{}/admin/settings", a.url))
                .header("cookie", session)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        assert_eq!(html.contains("action=\"/admin/settings/audit\""), allowed);
        assert!(html.contains("href=\"/admin/audit\""));
    }
}

#[tokio::test]
async fn fake_username_flood_does_not_block_another_ip_login() {
    let a = app().await;
    a.setup().await;
    for n in 0..1100 {
        let r = a
            .post(
                "/admin/login",
                "",
                &[("username", &format!("fake-{n}")), ("password", "wrong")],
            )
            .await;
        assert_eq!(r.status(), if n < 20 { 401 } else { 429 });
    }
    assert_eq!(
        a.post(
            "/admin/login",
            "",
            &[("username", "chief"), ("password", "owner-password")]
        )
        .await
        .status(),
        429
    );
    let other_ip = reqwest::Client::builder()
        .local_address(std::net::IpAddr::from([127, 0, 0, 2]))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(
        other_ip
            .post(format!("{}/admin/login", a.url))
            .form(&[("username", "chief"), ("password", "owner-password")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
}

#[tokio::test]
async fn dashboard_body_size_limit_rejects_before_mutation() {
    let a = app().await;
    let owner = a.setup().await;
    let response =
        a.c.post(format!("{}/admin/teams", a.url))
            .header("cookie", owner)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("name={}", "x".repeat(2 * 1024 * 1024)))
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), 413);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn slow_viewer_body_does_not_hold_mutation_lock_and_expires() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let a = app().await;
    let owner = a.setup().await;
    let reader = add(&a, &owner, "reader", "viewer").await;
    let viewer = a.login("reader", "account-password").await;
    let mut slow = tokio::net::TcpStream::connect(a.url.trim_start_matches("http://"))
        .await
        .unwrap();
    slow.write_all(format!("POST /admin/settings/password HTTP/1.1\r\nHost: localhost\r\nCookie: {viewer}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 512\r\nConnection: close\r\n\r\ncurrent_password=").as_bytes()).await.unwrap();
    // Give the accepted request a turn before the Owner operation arrives.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let deletion = tokio::time::timeout(
        Duration::from_secs(2),
        a.post(
            &format!("/admin/settings/accounts/{reader}/delete"),
            &owner,
            &[],
        ),
    )
    .await;
    assert_eq!(
        deletion
            .expect("An incomplete body must not hold the dashboard mutation lock.")
            .status(),
        303
    );
    let mut response = vec![0; 4096];
    let n = tokio::time::timeout(Duration::from_secs(16), slow.read(&mut response))
        .await
        .expect("Dashboard body deadline must return a response.")
        .unwrap();
    assert!(String::from_utf8_lossy(&response[..n]).starts_with("HTTP/1.1 408"));
}

#[tokio::test]
async fn setup_creates_named_owner_and_login_requires_that_account() {
    let a = app().await;
    a.setup().await;
    let (username, role, hash): (String, String, String) =
        sqlx::query_as("SELECT username,role,password_hash FROM admin_accounts")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
    assert_eq!((username.as_str(), role.as_str()), ("chief", "owner"));
    assert!(hash.starts_with("$argon2"));
    a.login("chief", "owner-password").await;
    assert_eq!(
        a.post("/admin/login", "", &[("password", "owner-password")])
            .await
            .status(),
        401
    );
}

async fn add(a: &App, owner: &str, username: &str, role: &str) -> i64 {
    assert_eq!(
        a.post(
            "/admin/settings/accounts",
            owner,
            &[
                ("username", username),
                ("role", role),
                ("password", "account-password")
            ]
        )
        .await
        .status(),
        303
    );
    sqlx::query_scalar("SELECT id FROM admin_accounts WHERE username=?")
        .bind(username)
        .fetch_one(&a.st.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn legacy_password_and_sessions_migrate_without_operator_action() {
    let dir = tempfile::tempdir().unwrap();
    let migrations = dir.path().join("old-migrations");
    std::fs::create_dir(&migrations).unwrap();
    for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_str()
            .unwrap()
            .split('_')
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            < 20
        {
            std::fs::copy(entry.path(), migrations.join(entry.file_name())).unwrap();
        }
    }
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite?mode=rwc", dir.path().display());
    let db = sqlx::SqlitePool::connect(&cfg.server.database)
        .await
        .unwrap();
    sqlx::migrate::Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&db)
        .await
        .unwrap();
    let hash = tokomotive::settings::hash_password("legacy-password").unwrap();
    sqlx::query("INSERT INTO settings(key,value) VALUES('admin_password_hash',?)")
        .bind(&hash)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions(token_hash,expires_at,password_hash) VALUES(?,?,?)")
        .bind(tokomotive::db::hash_key("old-session"))
        .bind(tokomotive::db::now_s() + 3600)
        .bind(&hash)
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    let db = tokomotive::db::connect(&cfg.server.database).await.unwrap();
    let (name, role, migrated): (String, String, String) =
        sqlx::query_as("SELECT username,role,password_hash FROM admin_accounts")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        (name.as_str(), role.as_str(), migrated.as_str()),
        ("admin", "owner", hash.as_str())
    );
    assert!(
        tokomotive::db::session_valid(&db, "old-session")
            .await
            .unwrap()
    );
    // Serve the upgraded database and exercise the backward-compatible form.
    let st = AppState::new(&cfg, db, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(axum::serve(listener, tokomotive::router(st)).into_future());
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{url}/admin/login"))
        .form(&[("password", "legacy-password")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    task.abort();
}

/// Derive the inventory from every router source, not a hand-picked test list.
/// The Owner probe also checks each registered method really reaches the router.
fn mutations() -> Vec<String> {
    let mut paths = vec![];
    fn sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    let mut files = vec![];
    sources(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/web")),
        &mut files,
    );
    for file in files {
        let text = std::fs::read_to_string(file).unwrap();
        assert!(
            !text.contains(".route_service("),
            "Extend the inventory before adding route services."
        );
        for route in text.split(".route(").skip(1) {
            let mut depth = 1;
            let mut quoted = false;
            let mut escaped = false;
            let end = route
                .char_indices()
                .find_map(|(i, c)| {
                    if escaped {
                        escaped = false;
                        return None;
                    }
                    if c == '\\' && quoted {
                        escaped = true;
                        return None;
                    }
                    if c == '"' {
                        quoted = !quoted;
                    }
                    if !quoted {
                        if c == '(' {
                            depth += 1;
                        } else if c == ')' {
                            depth -= 1;
                        }
                    }
                    (depth == 0).then_some(i)
                })
                .unwrap();
            let route = &route[..end];
            let (head, tail) = route.split_once('"').unwrap();
            assert!(
                head.trim().is_empty(),
                "Use literal paths so the role inventory remains complete."
            );
            let (path, handlers) = tail.split_once('"').unwrap();
            let handlers = handlers.split(".route(").next().unwrap();
            assert!(
                handlers.contains("get(") || handlers.contains("post("),
                "Extend the inventory for new routing methods."
            );
            assert!(
                !handlers.contains("put(")
                    && !handlers.contains("patch(")
                    && !handlers.contains("delete("),
                "Extend the inventory for new HTTP methods."
            );
            if handlers.contains("post(") {
                paths.push(path.into());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[tokio::test]
async fn every_mutating_route_enforces_the_role_matrix_and_hides_viewer_forms() {
    let a = app().await;
    tokomotive::settings::put(&a.st.db, "models_dev_url", "http://127.0.0.1:9/catalog")
        .await
        .unwrap();
    let owner = a.setup().await;
    add(&a, &owner, "reader", "viewer").await;
    add(&a, &owner, "operator", "admin").await;
    let viewer = a.login("reader", "account-password").await;
    let admin = a.login("operator", "account-password").await;
    let paths = mutations();
    assert!(paths.len() > 50);
    assert!(
        paths
            .iter()
            .any(|p| p == "/admin/providers/{name}/oauth/refresh")
    );
    for exception in [
        "/admin/login",
        "/setup",
        "/admin/logout",
        "/admin/settings/password",
    ] {
        assert!(paths.iter().any(|p| p == exception));
    }
    for path in paths {
        if matches!(
            path.as_str(),
            "/admin/login"
                | "/setup"
                | "/admin/logout"
                | "/admin/settings/password"
                | "/admin/routes/try"
        ) {
            continue;
        }
        let path = path
            .replace("{id}", "1")
            .replace("{name}", "fixture-provider")
            .replace("{index}", "0");
        assert_eq!(
            a.post(&path, &viewer, &[]).await.status(),
            403,
            "Viewer {path}"
        );
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
        let status = a.post(&path, &owner, &[]).await.status();
        assert_ne!(status, 404, "Route inventory drift: {path}");
        assert_ne!(status, 405, "Route inventory drift: {path}");
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
        assert_eq!(after, before + 1, "Missing audit entry: {path}");
    }
    assert_eq!(
        a.post("/admin/settings/accounts", &admin, &[])
            .await
            .status(),
        403
    );
    assert_eq!(
        a.post("/admin/settings", &admin, &[("metrics_token", "stolen")])
            .await
            .status(),
        403
    );
    // An egress proxy URL decides where Provider credentials go, like a base
    // URL of an env-backed Provider: Owner only.
    for path in [
        "/admin/settings/egress",
        "/admin/providers/fixture-provider/egress",
    ] {
        assert_eq!(
            a.post(
                path,
                &admin,
                &[("mode", "custom"), ("proxy_url", "http://127.0.0.1:9")]
            )
            .await
            .status(),
            403,
            "Admin {path}"
        );
    }
    assert_eq!(
        a.post("/admin/teams", &admin, &[("name", "allowed")])
            .await
            .status(),
        303
    );
    for (session, role) in [(&owner, "owner"), (&admin, "admin"), (&viewer, "viewer")] {
        let html =
            a.c.get(format!("{}/admin/settings", a.url))
                .header("cookie", session)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        assert_eq!(html.contains("<h2 id=\"accounts-h\">"), role == "owner");
        if role == "viewer" {
            assert!(!html.contains("action=\"/admin/settings/mode\""));
            assert!(html.contains("action=\"/admin/settings/password\""));
        }
    }
    assert_eq!(
        a.post(
            "/admin/settings/password",
            &viewer,
            &[
                ("current_password", "account-password"),
                ("password", "viewer-password")
            ]
        )
        .await
        .status(),
        303
    );
    let viewer = a.login("reader", "viewer-password").await;
    assert_eq!(a.post("/admin/logout", &viewer, &[]).await.status(), 303);
}

#[tokio::test]
async fn viewer_can_try_model_resolution_without_mutation_or_upstream_calls() {
    let a = app().await;
    let owner = a.setup().await;
    add(&a, &owner, "reader", "viewer").await;
    let viewer = a.login("reader", "account-password").await;
    let page =
        a.c.get(format!("{}/admin/routes", a.url))
            .header("cookie", &viewer)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(page.contains("Try a model name"));
    assert!(page.contains("action=\"/admin/routes/try\""));
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    let r = a
        .post("/admin/routes/try", &viewer, &[("model", "deepseek")])
        .await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("404"));
    assert!(html.contains("catalog Model or alias"));
    assert!(html.contains("model_not_found"));
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn last_owner_and_account_session_invalidation() {
    let a = app().await;
    let owner = a.setup().await;
    let chief: i64 = sqlx::query_scalar("SELECT id FROM admin_accounts WHERE username='chief'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{chief}/delete"),
            &owner,
            &[]
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{chief}"),
            &owner,
            &[("username", "chief"), ("role", "admin")]
        )
        .await
        .status(),
        400
    );
    let second = add(&a, &owner, "second", "owner").await;
    let old = a.login("second", "account-password").await;
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{second}"),
            &owner,
            &[("username", "second"), ("role", "admin")]
        )
        .await
        .status(),
        303
    );
    let r =
        a.c.get(format!("{}/admin", a.url))
            .header("cookie", &old)
            .send()
            .await
            .unwrap();
    assert_eq!(r.headers()["location"], "/admin/login");
    let old = a.login("second", "account-password").await;
    assert_eq!(
        a.post(
            "/admin/settings/password",
            &old,
            &[
                ("current_password", "account-password"),
                ("password", "new-password")
            ]
        )
        .await
        .status(),
        303
    );
    assert!(
        !tokomotive::db::session_valid(&a.st.db, old.split_once('=').unwrap().1)
            .await
            .unwrap()
    );
    assert!(
        tokomotive::db::session_valid(&a.st.db, owner.split_once('=').unwrap().1)
            .await
            .unwrap()
    );
    let old = a.login("second", "new-password").await;
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{second}/delete"),
            &owner,
            &[]
        )
        .await
        .status(),
        303
    );
    assert!(
        !tokomotive::db::session_valid(&a.st.db, old.split_once('=').unwrap().1)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn demoted_legacy_admin_cannot_regain_owner_by_changing_own_password() {
    let a = app().await;
    let r = a
        .post("/setup", "", &[("password", "legacy-password")])
        .await;
    assert_eq!(r.status(), 303);
    let original = cookie(&r);
    add(&a, &original, "chief", "owner").await;
    let owner = a.login("chief", "account-password").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM admin_accounts WHERE username='admin'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{id}"),
            &owner,
            &[("username", "admin"), ("role", "viewer")]
        )
        .await
        .status(),
        303
    );
    let viewer = a.login("admin", "legacy-password").await;
    assert_eq!(
        a.post(
            "/admin/settings/password",
            &viewer,
            &[
                ("current_password", "legacy-password"),
                ("password", "viewer-password")
            ]
        )
        .await
        .status(),
        303
    );
    let role: String = sqlx::query_scalar("SELECT role FROM admin_accounts WHERE id=?")
        .bind(id)
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(role, "viewer");
    let viewer = a.login("admin", "viewer-password").await;
    assert_eq!(
        a.post("/admin/teams", &viewer, &[("name", "forbidden")])
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn audit_records_mutations_without_secrets_and_has_filters_and_retention() {
    let a = app().await;
    let owner = a.setup().await;
    add(&a, &owner, "reader", "viewer").await;
    assert_eq!(
        a.post("/admin/teams", &owner, &[("name", "Team-a")])
            .await
            .status(),
        303
    );
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT action,before_json,after_json FROM admin_audit WHERE target_type='teams'",
    )
    .fetch_all(&a.st.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].1, rows[0].2);
    let target_id: String =
        sqlx::query_scalar("SELECT target_id FROM admin_audit WHERE target_type='teams'")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
    assert_eq!(
        target_id, "1",
        "The create audit entry must identify the new Team."
    );
    for secret in ["owner-password", "account-password"] {
        assert!(!format!("{rows:?}").contains(secret));
    }
    let r =
        a.c.get(format!(
            "{}/admin/audit?account=chief&action=POST&target=teams&limit=1",
            a.url
        ))
        .header("cookie", &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("Team-a"));
    assert!(!html.contains("account-password"));
    assert_eq!(
        a.post("/admin/teams", &owner, &[("name", "Team-b")])
            .await
            .status(),
        303
    );
    for (page, id) in [(1, 2), (2, 1)] {
        let html =
            a.c.get(format!(
                "{}/admin/audit?account=chief&target=teams&limit=1&page={page}",
                a.url
            ))
            .header("cookie", &owner)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(html.matches("class=\"audit-entry\"").count(), 1);
        assert!(html.contains("<table class=\"audit-table\">"));
        assert!(html.contains("<summary>Before and after"));
        assert!(html.contains(&format!("teams / {id}")));
        assert!(html.contains(if page == 1 { "page=2" } else { "page=1" }));
    }
    let html =
        a.c.get(format!("{}/admin/audit?since=2099-01-01T00:00", a.url))
            .header("cookie", &owner)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(html.contains("No audit entries match"));
    let viewer = a.login("reader", "account-password").await;
    assert_eq!(
        a.c.get(format!("{}/admin/audit", a.url))
            .header("cookie", viewer)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        a.post("/admin/settings/audit", &owner, &[("days", "7")])
            .await
            .status(),
        303
    );
    sqlx::query("UPDATE admin_audit SET time=? WHERE target_type='teams' AND target_id='1'")
        .bind(tokomotive::db::now_s() - 8 * 86400)
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        tokomotive::admin::purge_audit(&a.st.db, tokomotive::db::now_s())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        a.post("/admin/settings/audit", &owner, &[("days", "0")])
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn audit_purge_commits_bounded_batches_and_keeps_recent_entries() {
    let a = app().await;
    a.setup().await;
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO admin_audit(time,account,action,target_type,target_id,summary,before_json,after_json) SELECT 0,'chief','POST /admin/teams','teams',x,'HTTP 303.','null','null' FROM n")
        .execute(&a.st.db).await.unwrap();
    // Each tick commits at most 5000 rows. A later tick cannot undo that batch.
    sqlx::query("CREATE TRIGGER test_purge_failure BEFORE DELETE ON admin_audit WHEN OLD.time=0 AND (SELECT count(*) FROM admin_audit WHERE time=0)<=5001 BEGIN SELECT RAISE(ABORT,'test purge failure'); END")
        .execute(&a.st.db).await.unwrap();
    assert_eq!(
        tokomotive::admin::purge_audit(&a.st.db, tokomotive::db::now_s())
            .await
            .unwrap(),
        5000
    );
    assert!(
        tokomotive::admin::purge_audit(&a.st.db, tokomotive::db::now_s())
            .await
            .is_err()
    );
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit WHERE time=0")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(remaining, 5001);
    sqlx::query("DROP TRIGGER test_purge_failure")
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        tokomotive::admin::purge_audit(&a.st.db, tokomotive::db::now_s())
            .await
            .unwrap(),
        5000
    );
    assert_eq!(
        tokomotive::admin::purge_audit(&a.st.db, tokomotive::db::now_s())
            .await
            .unwrap(),
        1
    );
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(recent, 1);
}

#[tokio::test]
async fn audit_table_defaults_to_25_entries_and_keeps_explicit_page_size() {
    let a = app().await;
    let owner = a.setup().await;
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<30) INSERT INTO admin_audit(account,action,target_type,target_id,summary,before_json,after_json) SELECT 'chief','POST /admin/teams','teams',x,'HTTP 303. Public fields changed.','null','{}' FROM n")
        .execute(&a.st.db).await.unwrap();
    for (query, count) in [("?target=teams", 25), ("?target=teams&limit=10", 10)] {
        let html =
            a.c.get(format!("{}/admin/audit{query}", a.url))
                .header("cookie", &owner)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        assert_eq!(html.matches("class=\"audit-entry\"").count(), count);
        for column in ["Time (UTC)", "Account", "Action", "Target", "Result"] {
            assert!(html.contains(&format!("scope=\"col\">{column}</th>")));
        }
        assert!(html.contains("page=2"));
        assert!(html.contains("target=teams"));
        assert!(html.contains("role=\"region\" aria-label=\"Audit entries\" tabindex=\"0\""));
    }
}

#[tokio::test]
async fn audit_excludes_passwords_provider_keys_webhook_urls_client_secrets_and_tokens() {
    let a = app().await;
    let owner = a.setup().await;
    assert_eq!(
        a.post(
            "/admin/providers",
            &owner,
            &[
                ("name", "test"),
                ("kind", "openai"),
                ("base_url", "https://provider.example"),
                ("api_key", "provider-key-DO-NOT-LOG")
            ]
        )
        .await
        .status(),
        303
    );
    let credentials=a.st.secret_key.encrypt(r#"{"client_secret":"client-secret-DO-NOT-LOG","access_token":"access-token-DO-NOT-LOG","refresh_token":"refresh-token-DO-NOT-LOG"}"#).unwrap();
    sqlx::query("INSERT INTO oauth_credentials(provider,flow,secret) VALUES('test','codex',?)")
        .bind(&credentials)
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.post(
            "/admin/providers/test/keys",
            &owner,
            &[("api_key", "second-provider-key-DO-NOT-LOG")]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/settings/alerts",
            &owner,
            &[
                ("url", "https://webhook.example/secret-DO-NOT-LOG"),
                ("format", "generic"),
                ("enabled", "on")
            ]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/settings",
            &owner,
            &[
                ("upstream_timeout_secs", "120"),
                ("session_days", "7"),
                ("failure_threshold", "3"),
                ("cooldown_secs", "30"),
                ("max_attempts", "3"),
                ("metrics_token", "metrics-token-DO-NOT-LOG")
            ]
        )
        .await
        .status(),
        303
    );
    let mut runtime: serde_json::Value = serde_json::from_str(
        &tokomotive::settings::value(&a.st.db, "runtime_settings")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    runtime["new_secret_field"] = serde_json::json!("unknown-secret-DO-NOT-LOG");
    runtime["router"]["client_secret"] = serde_json::json!("nested-secret-DO-NOT-LOG");
    tokomotive::settings::put(&a.st.db, "runtime_settings", &runtime.to_string())
        .await
        .unwrap();
    assert_eq!(
        a.post("/admin/settings/audit", &owner, &[("days", "30")])
            .await
            .status(),
        303
    );
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM admin_accounts WHERE username='chief'")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
    let rows: Vec<(String, String, String, String, String)> =
        sqlx::query_as("SELECT account,action,summary,before_json,after_json FROM admin_audit")
            .fetch_all(&a.st.db)
            .await
            .unwrap();
    let log = format!("{rows:?}");
    for secret in [
        "DO-NOT-LOG",
        "owner-password",
        hash.as_str(),
        credentials.as_str(),
        "https://webhook.example",
    ] {
        assert!(!log.contains(secret), "Secret leaked: {secret}");
    }
    assert!(log.contains("credential_metadata"));
}

#[tokio::test]
async fn audit_keeps_public_card_tiers_and_rejects_stale_role_sessions() {
    let a = app().await;
    let owner = a.setup().await;
    let id = tokomotive::db::create_model(&a.st.db, "model", "provider", &[])
        .await
        .unwrap();
    assert_eq!(
        a.post(
            &format!("/admin/models/{id}/cost"),
            &owner,
            &[
                ("input", "1"),
                ("output", "2"),
                ("tiers", "200001 3 4 0 0 0")
            ]
        )
        .await
        .status(),
        303
    );
    let after: String = sqlx::query_scalar(
        "SELECT after_json FROM admin_audit WHERE target_type='models' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&a.st.db)
    .await
    .unwrap();
    let after: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        after["cards"][0]["spec"]["tiers"][0]["above_input_tokens"],
        200001
    );
    let id = add(&a, &owner, "second", "owner").await;
    let stale = tokomotive::admin::by_username(&a.st.db, "second")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        a.post(
            &format!("/admin/settings/accounts/{id}"),
            &owner,
            &[("username", "second"), ("role", "viewer")]
        )
        .await
        .status(),
        303
    );
    tokomotive::admin::create_session(&a.st.db, "late-login", 1, &stale)
        .await
        .unwrap();
    assert!(
        !tokomotive::db::session_valid(&a.st.db, "late-login")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn login_rate_limit_is_per_username_and_ignores_forged_forwarded_ips() {
    let a = app().await;
    a.setup().await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    for n in 0..6 {
        let r =
            a.c.post(format!("{}/admin/login", a.url))
                .header("x-forwarded-for", format!("192.0.2.{n}"))
                .form(&[("username", "chief"), ("password", "wrong")])
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), if n < 5 { 401 } else { 429 });
    }
    assert_eq!(
        a.post(
            "/admin/login",
            "",
            &[("username", "other"), ("password", "wrong")]
        )
        .await
        .status(),
        401
    );
    let other_ip = reqwest::Client::builder()
        .local_address(std::net::IpAddr::from([127, 0, 0, 2]))
        .build()
        .unwrap();
    assert_eq!(
        other_ip
            .post(format!("{}/admin/login", a.url))
            .form(&[("username", "chief"), ("password", "wrong")])
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "Unauthenticated attempts must not grow the audit log without bound."
    );
}
