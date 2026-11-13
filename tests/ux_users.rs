//! Users and Teams pages: labelled row editors, validation, Team management.

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
            .header("hx-request", "true")
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn user(&self, name: &str, team: Option<i64>) -> i64 {
        db::create_user_with_limits(&self.st.db, name, team, None, false, Limits::default())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn user_rows_have_one_labelled_edit_form() {
    let a = app().await;
    let id = a.user("maria.keller", None).await;
    let html = a.page("/admin/users").await;
    assert!(html.contains("<table class=\"users cards-sm\">"), "{html}");
    assert!(html.contains(&format!("id=\"u{id}\"")));
    assert!(html.contains("aria-label=\"Edit User maria.keller\""));
    assert!(html.contains("aria-label=\"Save User maria.keller\""));
    assert!(html.contains(&format!("<label for=\"u{id}-budget\">Budget (USD)</label>")));
    assert!(html.contains(&format!("<label for=\"u{id}-team\">Team</label>")));
    assert!(html.contains(&format!("href=\"/admin/keys?user={id}\"")));
    // Name, Team, Budget, monthly and the limits change in one save.
    let r = a
        .post(
            &format!("/admin/users/{id}"),
            &[
                ("name", "maria.k"),
                ("team_id", ""),
                ("budget_usd", "12.5"),
                ("monthly", "on"),
                ("rpm_limit", "10"),
                ("tpm_limit", ""),
            ],
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["hx-redirect"], format!("/admin/users#u{id}"));
    let u = &db::list_users(&a.st.db).await.unwrap()[0];
    assert_eq!(
        (
            u.name.as_str(),
            u.budget_micros,
            u.budget_monthly,
            u.rpm_limit
        ),
        ("maria.k", Some(12_500_000), true, Some(10))
    );
    let html = a.page("/admin/users").await;
    assert!(html.contains("$12.50"), "{html}");
    assert!(html.contains("this month"));
}

#[tokio::test]
async fn negative_budgets_and_limits_are_rejected_inline() {
    let a = app().await;
    let id = a.user("maria", None).await;
    for (field, value, message) in [
        (
            "budget_usd",
            "-5",
            "A Budget must be 0 or more, or blank for no Budget.",
        ),
        ("budget_usd", "10,5", "is not a dollar amount"),
        ("rpm_limit", "-1", "Rate Limits must be whole numbers"),
    ] {
        for path in ["/admin/users".to_owned(), format!("/admin/users/{id}")] {
            let r = a.post(&path, &[("name", "new-user"), (field, value)]).await;
            assert_eq!(r.status(), 400, "{path} {field}={value}");
            assert_eq!(r.headers()["hx-retarget"], "find .form-error");
            let body = r.text().await.unwrap();
            assert!(body.contains(message), "{path} {field}={value}: {body}");
        }
    }
    let u = &db::list_users(&a.st.db).await.unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].budget_micros, None);
    let html = a.page("/admin/users").await;
    assert!(html.contains("type=\"number\" min=\"0\" step=\"0.01\""));
}

#[tokio::test]
async fn duplicate_user_rename_is_a_plain_error() {
    let a = app().await;
    a.user("maria", None).await;
    let jonas = a.user("jonas", None).await;
    let r = a
        .post(&format!("/admin/users/{jonas}"), &[("name", "maria")])
        .await;
    assert_eq!(r.status(), 400);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("A User named maria already exists.")
    );
}

#[tokio::test]
async fn teams_list_members_and_overrides_and_can_be_renamed() {
    let a = app().await;
    let team = db::create_team(&a.st.db, "research").await.unwrap();
    a.user("maria", Some(team)).await;
    let html = a.page("/admin/teams").await;
    assert!(html.contains("<table class=\"teams cards-sm\">"), "{html}");
    assert!(html.contains("maria"));
    assert!(html.contains("aria-label=\"Edit Team research\""));
    let r = a
        .post(&format!("/admin/teams/{team}"), &[("name", "science")])
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(db::list_teams(&a.st.db).await.unwrap()[0].name, "science");
    // A Team with Users is not deleted.
    let html = a.page("/admin/teams").await;
    assert!(html.contains(&format!("action=\"/admin/teams/{team}/delete\"")));
    assert!(html.contains("data-confirm=\"Delete Team science?"));
    let r = a.post(&format!("/admin/teams/{team}/delete"), &[]).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Move its Users"));
    let empty = db::create_team(&a.st.db, "empty").await.unwrap();
    let r = a.post(&format!("/admin/teams/{empty}/delete"), &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(db::list_teams(&a.st.db).await.unwrap().len(), 1);
}
