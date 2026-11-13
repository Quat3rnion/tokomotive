//! Dashboard foundation: Setup login, error shape, flash, confirmation,
//! favicon, 404 page, first-run checklist and the proxy base URL.

use tokomotive::{config::Config, state::AppState};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
}

async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    App {
        _dir: dir,
        st,
        url,
        c,
    }
}

/// The `name=value` pairs of all Set-Cookie headers that set a value.
fn cookies(r: &reqwest::Response) -> String {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter(|kv| !kv.ends_with('='))
        .collect::<Vec<_>>()
        .join("; ")
}

fn session(cookie: &str) -> String {
    cookie
        .split("; ")
        .find(|kv| kv.starts_with("tokomotive_session="))
        .expect("session cookie")
        .to_owned()
}

impl App {
    /// Runs Setup and returns the session cookie that Setup sets.
    async fn setup(&self) -> String {
        let r = self
            .c
            .post(format!("{}/setup", self.url))
            .form(&[
                ("password", "long-password"),
                ("password_confirm", "long-password"),
                ("mode", "full"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303);
        session(&cookies(&r))
    }

    async fn get(&self, path: &str, cookie: &str) -> (u16, String) {
        let r = self
            .c
            .get(format!("{}{path}", self.url))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn post(&self, path: &str, cookie: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", cookie)
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn post_htmx(
        &self,
        path: &str,
        cookie: &str,
        form: &[(&str, &str)],
    ) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", cookie)
            .header("hx-request", "true")
            .form(form)
            .send()
            .await
            .unwrap()
    }
}

/// The opening tags of all `<form>` elements with their content.
fn forms(html: &str) -> Vec<&str> {
    html.split("<form")
        .skip(1)
        .map(|f| f.split("</form>").next().unwrap())
        .collect()
}

#[tokio::test]
async fn setup_page_explains_modes_and_confirms_the_password() {
    let a = app().await;
    let (status, html) = a.get("/setup", "").await;
    assert_eq!(status, 200);
    assert!(html.contains("<h1>Set up Tokomotive</h1>"), "{html}");
    assert!(html.contains("<legend>Mode</legend>"));
    assert!(html.contains("type=\"radio\" name=\"mode\" value=\"full\" checked"));
    assert!(html.contains("name=\"password_confirm\""));
    let r = a
        .post(
            "/setup",
            "",
            &[
                ("password", "long-password"),
                ("password_confirm", "other-password"),
            ],
        )
        .await;
    assert_eq!(r.status(), 400);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("The passwords do not match.")
    );
}

#[tokio::test]
async fn setup_logs_the_operator_in_and_says_so() {
    let a = app().await;
    let r = a
        .post(
            "/setup",
            "",
            &[
                ("password", "long-password"),
                ("password_confirm", "long-password"),
                ("mode", "full"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin");
    let cookie = cookies(&r);
    assert!(cookie.contains("tokomotive_session="), "{cookie}");
    assert!(cookie.contains("tokomotive_flash="), "{cookie}");
    let r =
        a.c.get(format!("{}/admin", a.url))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    let cleared = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .any(|v| v.to_str().unwrap().starts_with("tokomotive_flash=;"));
    assert!(cleared, "the flash is shown once");
    let html = r.text().await.unwrap();
    assert!(html.contains("role=\"status\""));
    assert!(html.contains("Setup is complete."), "{html}");
}

#[tokio::test]
async fn login_page_names_the_task_and_links_errors_to_the_field() {
    let a = app().await;
    a.setup().await;
    let (_, html) = a.get("/admin/login", "").await;
    assert!(html.contains("<h1>Log in</h1>"));
    let r = a.post("/admin/login", "", &[("password", "wrong")]).await;
    assert_eq!(r.status(), 401);
    let html = r.text().await.unwrap();
    assert!(html.contains("role=\"alert\" id=\"login-error\""), "{html}");
    assert!(html.contains("aria-describedby=\"login-error\""));
    assert!(html.contains("aria-invalid=\"true\""));
}

#[tokio::test]
async fn unknown_admin_url_is_a_styled_404_page() {
    let a = app().await;
    let cookie = a.setup().await;
    let r =
        a.c.get(format!("{}/admin/does-not-exist", a.url))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 404);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = r.text().await.unwrap();
    assert!(html.contains("<h1>Page not found</h1>"), "{html}");
    assert!(html.contains("href=\"/admin\""));
    assert!(html.contains("<nav aria-label=\"Main\">"));
    // Logged-out visitors see no admin navigation.
    let (status, html) = a.get("/admin/nothing-here", "").await;
    assert_eq!(status, 404);
    assert!(!html.contains("<nav aria-label=\"Main\">"));
    let (status, html) = a.get("/setup", "").await;
    assert_eq!(status, 403);
    assert!(html.contains("<h1>Setup is complete</h1>"), "{html}");
    assert!(html.contains("href=\"/admin/login\""));
    assert!(!html.contains("<nav aria-label=\"Main\">"));
    assert!(!html.contains("javascript:"));
}

#[tokio::test]
async fn favicon_is_served_and_linked() {
    let a = app().await;
    let cookie = a.setup().await;
    let r =
        a.c.get(format!("{}/favicon.ico", a.url))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/svg+xml");
    assert!(r.text().await.unwrap().starts_with("<svg"));
    let (_, html) = a.get("/admin", &cookie).await;
    assert!(html.contains("rel=\"icon\""));
}

#[tokio::test]
async fn form_errors_have_one_shape_for_htmx_and_plain_posts() {
    let a = app().await;
    let cookie = a.setup().await;

    // htmx: a small fragment for the form's own error slot. The form is not
    // replaced, so the input stays.
    let r = a.post_htmx("/admin/teams", &cookie, &[("name", " ")]).await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.headers()["hx-retarget"], "find .form-error");
    assert_eq!(r.headers()["hx-reswap"], "innerHTML");
    let body = r.text().await.unwrap();
    assert!(body.contains("A Team needs a name."), "{body}");
    assert!(!body.contains("<html") && !body.contains("<form"), "{body}");

    // Plain post: a full page with the message and a real link back.
    let r =
        a.c.post(format!("{}/admin/teams", a.url))
            .header("cookie", &cookie)
            .header("referer", format!("{}/admin/teams", a.url))
            .form(&[("name", " ")])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 400);
    let body = r.text().await.unwrap();
    assert!(body.contains("<html"));
    assert!(body.contains("role=\"alert\""));
    assert!(body.contains("A Team needs a name."));
    assert!(body.contains("href=\"/admin/teams\""), "{body}");
    assert!(!body.contains("javascript:"));

    // A form that does not deserialize gets a plain sentence, not serde text.
    let r = a
        .post_htmx("/admin/models/1/price", &cookie, &[("x", "1")])
        .await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.headers()["hx-retarget"], "find .form-error");
    let body = r.text().await.unwrap();
    assert!(!body.contains("deserialize"), "{body}");
    assert!(body.contains("The form is incomplete."), "{body}");

    // No Provider yet: say what to do first.
    let r = a
        .post_htmx("/admin/models", &cookie, &[("name", "gpt-4o")])
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Add a Provider first."));

    // Unique violations name the duplicate and hide the SQL error.
    assert_eq!(
        a.post("/admin/teams", &cookie, &[("name", "research")])
            .await
            .status(),
        303
    );
    let r = a
        .post_htmx("/admin/teams", &cookie, &[("name", "research")])
        .await;
    assert_eq!(r.status(), 400);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("A Team named research already exists."),
        "{body}"
    );
    assert!(!body.contains("UNIQUE"));
}

#[tokio::test]
async fn saves_show_a_flash_message_once() {
    let a = app().await;
    let cookie = a.setup().await;
    let r = a
        .post("/admin/teams", &cookie, &[("name", "research")])
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/teams");
    let flash = cookies(&r);
    assert!(flash.contains("tokomotive_flash="));
    let (_, html) = a.get("/admin/teams", &format!("{cookie}; {flash}")).await;
    assert!(html.contains("Team research added."), "{html}");
    let (_, html) = a.get("/admin/teams", &cookie).await;
    assert!(!html.contains("Team research added."));

    // htmx saves carry the flash cookie with HX-Redirect.
    let r = a
        .post_htmx(
            "/admin/providers",
            &cookie,
            &[
                ("name", "openai"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "sk-test-1234"),
            ],
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["hx-redirect"], "/admin/providers");
    assert!(cookies(&r).contains("tokomotive_flash="));
}

#[tokio::test]
async fn destructive_actions_ask_for_confirmation() {
    let a = app().await;
    let cookie = a.setup().await;
    for form in [
        vec![
            ("name", "openai"),
            ("kind", "openai"),
            ("base_url", "http://127.0.0.1:9"),
            ("api_key", "sk-test-1234"),
        ],
        vec![
            ("name", "chatgpt"),
            ("kind", "openai"),
            ("auth", "chatgpt"),
            ("base_url", "https://api.openai.com"),
        ],
    ] {
        let r = a.post("/admin/providers", &cookie, &form).await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    }
    let r = a
        .post(
            "/admin/routes",
            &cookie,
            &[
                ("name", "fast"),
                ("strategy", "priority"),
                ("fallbacks", ""),
                ("deployment_provider", "openai"),
                ("deployment_model", "gpt-4o-mini"),
                ("deployment_weight", "1"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    assert_eq!(
        a.post("/admin/teams", &cookie, &[("name", "research")])
            .await
            .status(),
        303
    );
    let r = a
        .post(
            "/admin/models",
            &cookie,
            &[("name", "gpt-4o"), ("provider", "openai"), ("input", "1")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let model = r.headers()["location"].to_str().unwrap().to_owned();
    let team = tokomotive::db::list_teams(&a.st.db).await.unwrap()[0]
        .id
        .to_string();
    let r = a
        .post(
            &format!("{model}/team-price"),
            &cookie,
            &[
                ("team_id", team.as_str()),
                ("mode", "markup"),
                ("markup_percent", "10"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(
        a.post("/admin/users", &cookie, &[("name", "maria")])
            .await
            .status(),
        303
    );
    let user = tokomotive::db::list_users(&a.st.db).await.unwrap()[0]
        .id
        .to_string();
    a.post("/admin/keys", &cookie, &[("user_id", user.as_str())])
        .await;

    let mut destructive = 0;
    for page in ["/admin/providers", "/admin/routes", &model, "/admin/keys"] {
        let (status, html) = a.get(page, &cookie).await;
        assert_eq!(status, 200, "{page}");
        for form in forms(&html) {
            let action = form.split("action=\"").nth(1).unwrap_or("");
            let action = action.split('"').next().unwrap();
            let removes_override =
                action.ends_with("/team-price") && form.contains("value=\"default\"");
            if action.ends_with("/delete")
                || action.ends_with("/revoke")
                || action.ends_with("/disconnect")
                || removes_override
            {
                destructive += 1;
                assert!(
                    form.contains("data-confirm=\""),
                    "{page}: {action} has no confirmation"
                );
            }
        }
    }
    // Remove key, Delete provider x2, Disconnect x2, Delete route, Remove
    // override, Revoke.
    assert!(destructive >= 8, "{destructive}");
}

fn step(html: &str, name: &str) -> Option<bool> {
    let marker = format!("data-step=\"{name}\" data-done=\"");
    let rest = html.split(&marker).nth(1)?;
    Some(rest.starts_with("true"))
}

#[tokio::test]
async fn overview_checklist_follows_the_database() {
    let a = app().await;
    let cookie = a.setup().await;
    let (_, html) = a.get("/admin", &cookie).await;
    for name in ["provider", "model", "user", "key", "request"] {
        assert_eq!(step(&html, name), Some(false), "{name}: {html}");
    }
    assert!(html.contains("href=\"/admin/providers\""));
    a.post(
        "/admin/providers",
        &cookie,
        &[
            ("name", "openai"),
            ("kind", "openai"),
            ("base_url", "http://127.0.0.1:9"),
            ("api_key", "sk-test-1234"),
        ],
    )
    .await;
    a.post("/admin/users", &cookie, &[("name", "maria")]).await;
    let (_, html) = a.get("/admin", &cookie).await;
    assert_eq!(step(&html, "provider"), Some(true));
    assert_eq!(step(&html, "user"), Some(true));
    assert_eq!(step(&html, "key"), Some(false));
    let user = tokomotive::db::list_users(&a.st.db).await.unwrap()[0].id;
    let (_, key) = tokomotive::db::create_key_with_limits(
        &a.st.db,
        user,
        "",
        None,
        tokomotive::ratelimit::Limits::default(),
    )
    .await
    .unwrap();
    let (_, html) = a.get("/admin", &cookie).await;
    assert_eq!(step(&html, "key"), Some(true));
    // A request through the Proxy completes the list, so it goes away.
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", a.url))
        .bearer_auth(&key)
        .json(&serde_json::json!({"model": "openai/gpt-4o", "messages": []}))
        .send()
        .await
        .unwrap();
    a.st.flush().await;
    a.post(
        "/admin/routes",
        &cookie,
        &[
            ("name", "fast"),
            ("strategy", "priority"),
            ("deployment_provider", "openai"),
            ("deployment_model", "gpt-4o-mini"),
            ("deployment_weight", "1"),
        ],
    )
    .await;
    let (_, html) = a.get("/admin", &cookie).await;
    assert_eq!(step(&html, "model"), None, "{html}");
}

#[tokio::test]
async fn proxy_base_url_is_shown_with_copy_and_an_example() {
    let a = app().await;
    let cookie = a.setup().await;
    let host = a.url.trim_start_matches("http://").to_owned();
    let r =
        a.c.get(format!("{}/admin", a.url))
            .header("cookie", &cookie)
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .unwrap();
    let html = r.text().await.unwrap();
    assert!(html.contains(&format!("https://{host}/v1")), "{html}");
    assert!(html.contains("data-copy="));

    a.post("/admin/users", &cookie, &[("name", "maria")]).await;
    let r = a.post("/admin/keys", &cookie, &[("user_id", "1")]).await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("sk-toko-"));
    assert!(html.contains(&format!("http://{host}/v1/chat/completions")));
    assert!(html.contains("curl "));
    assert!(html.contains("data-copy="));
}

#[tokio::test]
async fn navigation_and_tabs_mark_the_current_page() {
    let a = app().await;
    let cookie = a.setup().await;
    let (_, html) = a.get("/admin/users", &cookie).await;
    assert!(html.contains("href=\"/admin/users\" aria-current=\"page\""));
    assert!(html.contains("href=\"#main\""), "skip link");
    let (_, html) = a.get("/admin?range=7d", &cookie).await;
    assert!(html.contains("href=\"/admin?range=7d\" aria-current=\"page\""));
    assert!(html.contains("<title>Overview - Tokomotive</title>"));
}

#[tokio::test]
async fn overview_charts_have_text_alternatives_and_refresh_is_opt_in() {
    let a = app().await;
    let cookie = a.setup().await;
    let (_, html) = a.get("/admin", &cookie).await;
    assert!(
        html.contains("role=\"img\" aria-label=\"Requests"),
        "{html}"
    );
    assert!(
        html.contains("<caption>Requests"),
        "data table for the chart"
    );
    assert!(!html.contains("every 15s"), "no forced polling");
    assert!(html.contains("id=\"auto-refresh\""));
    assert!(!html.contains("id=\"auto-refresh\" checked"));
}
