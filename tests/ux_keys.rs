//! Keys page: key setup, one edit form per key, read-only revoked keys.

use tokomotive::{config::Config, db, ratelimit::Limits, state::AppState};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
    cookie: String,
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
    let r = c
        .post(format!("{url}/setup"))
        .form(&[("password", "long-password"), ("mode", "full")])
        .send()
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("tokomotive_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    App {
        _dir: dir,
        st,
        url,
        c,
        cookie,
    }
}

impl App {
    async fn page(&self, path: &str) -> String {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn providers(&self) {
        for form in [
            vec![
                ("name", "openai"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "sk-test-1234"),
            ],
            vec![
                ("name", "claude-max"),
                ("kind", "anthropic"),
                ("auth", "claude"),
                ("base_url", "https://api.anthropic.com"),
            ],
        ] {
            assert_eq!(self.post("/admin/providers", &form).await.status(), 303);
        }
    }

    async fn user(&self, name: &str) -> i64 {
        db::create_user_with_limits(&self.st.db, name, None, None, false, Limits::default())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn without_users_the_page_says_to_add_one_first() {
    let a = app().await;
    let html = a.page("/admin/keys").await;
    assert!(
        html.contains("Add a User before you add a Virtual Key."),
        "{html}"
    );
    assert!(html.contains("href=\"/admin/users\""));
    assert!(!html.contains("name=\"user_id\""), "no empty User select");
}

#[tokio::test]
async fn add_form_marks_subscription_providers_and_suggests_models() {
    let a = app().await;
    a.providers().await;
    a.user("maria").await;
    let html = a.page("/admin/keys").await;
    assert!(html.contains("A Subscription Provider needs a check."));
    assert!(html.contains("type=\"checkbox\" name=\"allowed_providers\" value=\"claude-max\""));
    assert!(html.contains("claude-max <span class=\"tag info\">subscription</span>"));
    assert!(html.contains("name=\"allowed_models\" data-search=\"models\" data-csv"));
    assert!(html.contains("<button>Add key</button>"));
}

#[tokio::test]
async fn provider_access_uses_checkboxes_through_thirty_and_accepts_both_forms() {
    let a = app().await;
    a.providers().await;
    let user = a.user("maria").await;
    let (id, _) = db::create_key(&a.st.db, user, "laptop", None)
        .await
        .unwrap();
    for n in 2..30 {
        let name = format!("provider-{n:02}");
        assert_eq!(
            a.post(
                "/admin/providers",
                &[
                    ("name", &name),
                    ("kind", "openai"),
                    ("base_url", "http://127.0.0.1:9"),
                    ("api_key", "fixture")
                ]
            )
            .await
            .status(),
            303
        );
    }
    for values in [
        vec![
            ("allowed_providers", "openai"),
            ("allowed_providers", "claude-max"),
        ],
        vec![("allowed_providers", "openai, claude-max")],
    ] {
        assert_eq!(
            a.post(&format!("/admin/keys/{id}"), &values).await.status(),
            303
        );
    }
    let html = a.page("/admin/keys").await;
    assert_eq!(
        html.matches("type=\"checkbox\" name=\"allowed_providers\"")
            .count(),
        60
    );
    assert!(html.contains("value=\"claude-max\" checked"));
    assert!(!html.contains("name=\"allowed_providers\" data-search="));
    assert_eq!(
        a.post(
            "/admin/providers",
            &[
                ("name", "provider-31"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
                ("api_key", "fixture")
            ]
        )
        .await
        .status(),
        303
    );
    let html = a.page("/admin/keys").await;
    assert!(!html.contains("type=\"checkbox\" name=\"allowed_providers\""));
    assert!(html.contains("name=\"allowed_providers\" data-search=\"providers\" data-csv"));
    assert_eq!(
        a.post(
            &format!("/admin/keys/{id}"),
            &[("allowed_providers", "openai, claude-max")]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            &format!("/admin/keys/{id}"),
            &[
                ("allowed_providers", "openai"),
                ("allowed_providers", "claude-max")
            ]
        )
        .await
        .status(),
        303
    );
}

#[tokio::test]
async fn small_team_and_provider_sets_use_selects_with_a_bounded_fallback() {
    let a = app().await;
    a.providers().await;
    a.user("maria").await;
    let model = db::create_model(&a.st.db, "fixture-model", "openai", &[])
        .await
        .unwrap();
    a.st.reload_catalog().await.unwrap();
    for n in 0..30 {
        db::create_team(&a.st.db, &format!("Team {n:02}"))
            .await
            .unwrap();
    }
    assert!(
        a.page("/admin/users")
            .await
            .contains("<select id=\"new-team\" name=\"team_name\"")
    );
    assert!(
        a.page(&format!("/admin/models/{model}"))
            .await
            .contains("<select id=\"t-team\" name=\"team_name\"")
    );
    assert!(
        a.page(&format!("/admin/models/{model}"))
            .await
            .contains("<select id=\"m-provider\" name=\"provider\"")
    );
    assert!(
        a.page("/admin/models")
            .await
            .contains("<select id=\"new-provider\" name=\"provider\"")
    );
    assert!(
        a.page("/admin/routes")
            .await
            .contains("<select name=\"deployment_provider\"")
    );
    assert!(
        a.page("/admin/ledger")
            .await
            .contains("<select id=\"lf-provider\" name=\"provider\"")
    );
    db::create_team(&a.st.db, "Team 31").await.unwrap();
    assert!(
        a.page("/admin/users")
            .await
            .contains("<input id=\"new-team\" name=\"team_name\" data-search=\"teams\"")
    );
    assert!(
        a.page(&format!("/admin/models/{model}"))
            .await
            .contains("<input id=\"t-team\" name=\"team_name\" data-search=\"teams\"")
    );
    // Selects submit exact names, just like the large-set input.
    assert_eq!(
        a.post(
            "/admin/users",
            &[("name", "select-user"), ("team_name", "Team 00")]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        a.post(
            "/admin/users",
            &[("name", "input-user"), ("team_name", "Team 31")]
        )
        .await
        .status(),
        303
    );
}

#[tokio::test]
async fn list_filters_follow_the_heading_and_intro_and_are_not_panels() {
    let a = app().await;
    a.providers().await;
    for path in ["users", "teams", "keys", "models", "routes", "providers"] {
        let html = a.page(&format!("/admin/{path}")).await;
        let content = html.split("<h1>").nth(1).unwrap();
        let intro = content.find("</p>").unwrap();
        let filter = content.find("aria-label=\"Search by name\"").unwrap();
        assert!(intro < filter, "{path}: intro must precede search");
        assert!(
            html.contains("<form method=\"get\" class=\"row list-filter\""),
            "{path}: compact toolbar"
        );
    }
}

#[tokio::test]
async fn unknown_allowed_models_are_rejected() {
    let a = app().await;
    a.providers().await;
    let user = a.user("maria").await.to_string();
    let r = a
        .post(
            "/admin/keys",
            &[("user_id", user.as_str()), ("allowed_models", "nope-model")],
        )
        .await;
    assert_eq!(r.status(), 400);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("Unknown model: nope-model.")
    );
    let r = a
        .post(
            "/admin/keys",
            &[
                ("user_id", user.as_str()),
                ("allowed_models", "openai/gpt-4o"),
            ],
        )
        .await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn one_form_saves_limits_and_provider_access() {
    let a = app().await;
    a.providers().await;
    let user = a.user("maria").await;
    let (id, _) = db::create_key_with_limits(&a.st.db, user, "laptop", None, Limits::default())
        .await
        .unwrap();
    let html = a.page("/admin/keys").await;
    assert!(html.contains("All API-key Providers"), "{html}");
    assert!(html.contains(&format!("action=\"/admin/keys/{id}\"")));
    assert!(html.contains("aria-label=\"Edit key sk-toko-"));
    assert!(html.contains("aria-label=\"Revoke key sk-toko-"));
    let r = a
        .post(
            &format!("/admin/keys/{id}"),
            &[
                ("rpm_limit", "5"),
                ("tpm_limit", ""),
                ("allowed_providers", "claude-max"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], format!("/admin/keys#k{id}"));
    let key = &db::list_keys(&a.st.db).await.unwrap()[0];
    assert_eq!((key.rpm_limit, key.tpm_limit), (Some(5), None));
    assert_eq!(key.allowed_providers.as_deref(), Some("[\"claude-max\"]"));
    let html = a.page("/admin/keys").await;
    assert!(html.contains("RPM 5, TPM unlimited"), "{html}");
}

#[tokio::test]
async fn revoked_keys_are_read_only_and_listed_apart() {
    let a = app().await;
    let user = a.user("maria").await;
    let (id, _) = db::create_key_with_limits(&a.st.db, user, "old", None, Limits::default())
        .await
        .unwrap();
    db::revoke_key(&a.st.db, id).await.unwrap();
    let html = a.page("/admin/keys").await;
    assert!(html.contains("Revoked keys (1)"), "{html}");
    assert!(!html.contains(&format!("action=\"/admin/keys/{id}\"")));
    assert!(!html.contains(&format!("action=\"/admin/keys/{id}/revoke\"")));
}

#[tokio::test]
async fn user_filter_lists_their_keys_and_preselects_them() {
    let a = app().await;
    let maria = a.user("maria").await;
    let jonas = a.user("jonas").await;
    for (u, label) in [(maria, "maria-laptop"), (jonas, "jonas-laptop")] {
        db::create_key_with_limits(&a.st.db, u, label, None, Limits::default())
            .await
            .unwrap();
    }
    let html = a.page(&format!("/admin/keys?user={jonas}")).await;
    assert!(html.contains("jonas-laptop"));
    assert!(!html.contains("maria-laptop"));
    assert!(html.contains("name=\"user_name\" value=\"jonas\" data-search=\"users\""));
    assert!(html.contains("href=\"/admin/keys\""), "link to all keys");
}
