//! Pagination is tested through the database and the HTTP dashboard/API seams.
use serde_json::Value;
use std::collections::BTreeSet;
use tokomotive::{config::Config, db, state::AppState};
use tokomotive::{
    pricing::{CardRates, PriceSpec},
    usage::Usage,
};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    client: reqwest::Client,
    token: String,
}

impl App {
    async fn large() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut text = String::new();
        for n in 0..251 {
            text += &format!(
                "[[providers]]\nname='provider-{n:03}'\nkind='openai'\nbase_url='http://127.0.0.1:9'\napi_key='test'\n[[routes]]\nname='route-{n:03}'\n[[routes.deployments]]\nprovider='provider-{n:03}'\nmodel='upstream'\n"
            );
        }
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.providers[0].api_keys = (0..120).map(|n| format!("provider-key-{n:03}")).collect();
        let deployment = cfg.routes[0].deployments[0].clone();
        cfg.routes[0]
            .deployments
            .extend(std::iter::repeat_n(deployment, 120));
        cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
        let st = tokomotive::init(&cfg, None).await.unwrap();
        tokomotive::settings::put(&st.db, "admin_password_hash", "fixture-password-hash")
            .await
            .unwrap();
        db::create_session(&st.db, "pagination-session", 1)
            .await
            .unwrap();
        let mut users = vec![];
        let mut keys = vec![];
        let mut token = String::new();
        for n in 0..251 {
            let t = db::create_team(&st.db, &format!("Team {n:03}"))
                .await
                .unwrap();
            let u = db::create_user(&st.db, &format!("User {n:03}"), Some(t), None, false)
                .await
                .unwrap();
            let (k, plain) = db::create_key(&st.db, u, &format!("Key {n:03}"), None)
                .await
                .unwrap();
            if n == 0 {
                token = plain;
            }
            let (revoked, _) = db::create_key(&st.db, u, &format!("Revoked {n:03}"), None)
                .await
                .unwrap();
            db::revoke_key(&st.db, revoked).await.unwrap();
            let m = db::create_model(
                &st.db,
                &format!("model-{n:03}"),
                &format!("provider-{n:03}"),
                &[],
            )
            .await
            .unwrap();
            db::add_cost_card(&st.db, m, &CardRates::default())
                .await
                .unwrap();
            if n < 61 {
                db::add_price_card(&st.db, 1, Some(t), &PriceSpec::MarkupBp(1000))
                    .await
                    .unwrap();
            }
            users.push((u, t));
            keys.push(k);
        }
        for _ in 0..80 {
            db::add_cost_card(&st.db, 1, &CardRates::default())
                .await
                .unwrap();
        }
        st.reload_catalog().await.unwrap();
        for n in 0..1025 {
            let i = n % 251;
            let model = format!("model-{i:03}");
            let usage = Usage {
                input: (i + 1) as i64,
                output: 2,
                ..Default::default()
            };
            let quote = st.catalog().quote(Some(&model), Some(users[i].1), &usage);
            db::insert_entry(
                &st.db,
                &db::NewEntry {
                    session_affinity: None,
                    reported_cost_micros: None,
                    ts_ms: db::now_ms(),
                    user_id: users[i].0,
                    team_id: Some(users[i].1),
                    key_id: keys[i],
                    provider: format!("provider-{i:03}"),
                    subscription: false,
                    endpoint: "chat_completions".into(),
                    requested_model: Some(model.clone()),
                    model: Some(model),
                    status: 200,
                    streamed: false,
                    usage,
                    quote,
                    estimated: false,
                    latency_ms: i as i64,
                    ttft_ms: None,
                    client_aborted: false,
                    provider_key: None,
                    attempts: 1,
                },
            )
            .await
            .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = tokomotive::router(st.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            _dir: dir,
            st,
            url,
            client: reqwest::Client::new(),
            token,
        }
    }
    async fn small() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(
            r#"
[[providers]]
name='openai'
kind='openai'
base_url='http://127.0.0.1:9'
api_key='fixture'
[[providers]]
name='claude-max'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
[[teams]]
name='research'
[[teams]]
name='support'
[[models]]
name='gpt-4o'
provider='openai'
[models.cost]
input=1
output=2
[[routes]]
name='fast'
[[routes.deployments]]
provider='openai'
model='gpt-4o'
"#,
        )
        .unwrap();
        cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
        let st = tokomotive::init(&cfg, None).await.unwrap();
        tokomotive::settings::put(&st.db, "admin_password_hash", "fixture-password-hash")
            .await
            .unwrap();
        db::create_session(&st.db, "pagination-session", 1)
            .await
            .unwrap();
        let user = db::create_user(&st.db, "maria.keller", Some(1), None, false)
            .await
            .unwrap();
        db::create_user(&st.db, "jonas.meier", Some(2), None, false)
            .await
            .unwrap();
        let (key, token) = db::create_key(&st.db, user, "laptop", None).await.unwrap();
        db::update_key_providers(&st.db, key, &["openai".into(), "claude-max".into()])
            .await
            .unwrap();
        db::add_price_card(&st.db, 1, Some(1), &PriceSpec::MarkupBp(1000))
            .await
            .unwrap();
        st.reload_catalog().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = tokomotive::router(st.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            _dir: dir,
            st,
            url,
            client: reqwest::Client::new(),
            token,
        }
    }

    async fn page(&self, path: &str) -> String {
        let response = self
            .client
            .get(format!("{}{path}", self.url))
            .header("cookie", "tokomotive_session=pagination-session")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        response.text().await.unwrap()
    }
    async fn models(&self, query: &str, anthropic: bool) -> Value {
        let mut r = self
            .client
            .get(format!("{}/v1/models{query}", self.url))
            .bearer_auth(&self.token);
        if anthropic {
            r = r.header("anthropic-version", "2023-06-01");
        }
        r.send().await.unwrap().json().await.unwrap()
    }
}

fn ids(html: &str, prefix: &str) -> Vec<i64> {
    html.split(&format!("id=\"{prefix}"))
        .skip(1)
        .filter_map(|s| s.split('"').next()?.parse().ok())
        .collect()
}

fn page_link(html: &str, relation: &str) -> Option<String> {
    let html = html.rsplit("<nav class=\"pager\"").next()?;
    let marker = format!(" rel=\"{relation}\"");
    let at = html.find(&marker)?;
    Some(
        html[..at]
            .rsplit("href=\"")
            .next()?
            .trim_end_matches('"')
            .replace("&#38;", "&")
            .replace("&amp;", "&"),
    )
}

fn pager_area<'a>(html: &'a str, prefix: &str) -> &'a str {
    match prefix {
        "k" => html.split("id=\"revoked-keys\"").next().unwrap(),
        "rk" => html.split("id=\"revoked-keys\"").nth(1).unwrap(),
        "card-" => html.split("id=\"history-h\"").nth(1).unwrap(),
        "override-" => html
            .split("id=\"team-prices\"")
            .nth(1)
            .unwrap()
            .split("<h3")
            .next()
            .unwrap(),
        _ => html,
    }
}

async fn walk(a: &App, path: &str, prefix: &str, limit: usize) -> BTreeSet<i64> {
    let mut found = BTreeSet::new();
    let mut url = path.to_owned();
    let mut previous: Option<Vec<i64>> = None;
    for _ in 0..100 {
        let html = a.page(&url).await;
        let current = ids(&html, prefix);
        assert!(current.len() <= limit, "{url}: {}", current.len());
        for id in &current {
            assert!(found.insert(*id), "duplicate {id}: {url}");
        }
        if let Some(expected) = previous {
            let prev = page_link(pager_area(&html, prefix), "prev").expect("Previous link");
            assert_eq!(ids(&a.page(&prev).await, prefix), expected, "{prev}");
        }
        match page_link(pager_area(&html, prefix), "next") {
            Some(next) => {
                previous = Some(current);
                url = next;
            }
            None => return found,
        }
    }
    panic!("pager did not terminate");
}

#[tokio::test]
async fn dashboard_pages_search_filters_and_clamps() {
    let a = App::large().await;
    for (path, prefix, total, limit) in [
        ("/admin/users", "u", 251, 50),
        ("/admin/teams", "t", 251, 50),
        ("/admin/models", "m", 251, 50),
        ("/admin/ledger", "e", 1025, 100),
    ] {
        let all = walk(&a, path, prefix, limit).await;
        assert_eq!(all.len(), total, "{path}");
        assert_eq!(all, BTreeSet::from_iter(1..=total as i64), "{path}");
        let clamped = a.page(&format!("{path}?limit=100000")).await;
        assert_eq!(ids(&clamped, prefix).len(), 200, "{path}");
    }
    // Each independent section supports both directions without gaps.
    let active = walk(&a, "/admin/keys", "k", 50).await;
    let revoked = walk(&a, "/admin/keys", "rk", 50).await;
    assert_eq!(active, BTreeSet::from_iter((1..=502).step_by(2)));
    assert_eq!(revoked, BTreeSet::from_iter((2..=502).step_by(2)));
    let mixed = a
        .page("/admin/keys?after=99&revoked_after=100&limit=10")
        .await;
    assert_eq!(ids(&mixed, "k")[0], 101);
    assert_eq!(ids(&mixed, "rk")[0], 102);
    assert!(
        page_link(pager_area(&mixed, "k"), "next")
            .unwrap()
            .contains("revoked_after=100")
    );
    assert!(
        page_link(pager_area(&mixed, "rk"), "next")
            .unwrap()
            .contains("after=99")
    );
    assert!(mixed.contains("id=\"revoked-keys\" open"));
    let clamped = a.page("/admin/keys?limit=100000").await;
    assert_eq!(ids(&clamped, "k").len(), 200);
    assert_eq!(ids(&clamped, "rk").len(), 200);
    assert_eq!(
        db::ledger_page(&a.st.db, &db::LedgerQuery::default())
            .await
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        db::ledger_page(
            &a.st.db,
            &db::LedgerQuery {
                limit: 100000,
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .len(),
        200
    );
    assert_eq!(walk(&a, "/admin/models/1", "card-", 20).await.len(), 142);
    assert_eq!(walk(&a, "/admin/models/1", "override-", 20).await.len(), 61);
    for (path, prefix) in [
        ("users", "u"),
        ("teams", "t"),
        ("keys", "k"),
        ("models", "m"),
    ] {
        let html = a.page(&format!("/admin/{path}?q=042")).await;
        assert_eq!(ids(&html, prefix).len(), 1, "{path}");
    }
    let filtered = a.page("/admin/users?team=1&limit=1&q=User").await;
    assert_eq!(ids(&filtered, "u"), vec![1]);
    let ledger = a
        .page("/admin/ledger?user=1&provider=provider-000&model=model-000&limit=2&filter=all")
        .await;
    let next = page_link(&ledger, "next").unwrap();
    for filter in [
        "user=1",
        "provider=provider-000",
        "model=model-000",
        "limit=2",
        "filter=all",
    ] {
        assert!(next.contains(filter), "{next}");
    }
    let html = a.page("/admin/users?q=User&limit=7").await;
    let next = page_link(&html, "next").unwrap();
    assert!(next.contains("q=User") && next.contains("limit=7"));
    assert_eq!(ids(&a.page(&next).await, "u").len(), 7);
    let search: Value = serde_json::from_str(&a.page("/admin/search/users?q=User").await).unwrap();
    assert_eq!(search.as_array().unwrap().len(), 20);
    assert_eq!(
        serde_json::from_str::<Value>(&a.page("/admin/search/users?q=042").await).unwrap()[0]["name"],
        "User 042"
    );
    let history = a.page("/admin/models/1").await;
    assert_eq!(ids(&history, "card-").len(), 20);
    assert_eq!(ids(&history, "override-").len(), 20);
    // History and overrides use separate cursors.
    let next = page_link(history.split("id=\"history-h\"").nth(1).unwrap(), "next").unwrap();
    assert_eq!(ids(&a.page(&next).await, "card-").len(), 20);
    assert_eq!(
        ids(
            &a.page("/admin/models/1?overrides_after=20").await,
            "override-"
        )
        .len(),
        20
    );
    assert_eq!(ids(&a.page("/admin/models?team=1").await, "m"), vec![1]);
}

#[tokio::test]
async fn overview_usage_and_configuration_lists_are_bounded() {
    let a = App::large().await;
    let overview = a.page("/admin?range=24h").await;
    assert_eq!(overview.matches(">Show all</a>").count(), 4);
    for group in ["model", "provider", "user", "team"] {
        let section = overview
            .split(&format!("aria-labelledby=\"by-{group}-h\""))
            .nth(1)
            .unwrap()
            .split("</table>")
            .next()
            .unwrap();
        assert_eq!(section.matches("<tr><th scope=\"row\">").count(), 10);
        let all = walk_usage(&a, group).await;
        let prefix = match group {
            "model" => "model-",
            "provider" => "provider-",
            "user" => "User ",
            _ => "Team ",
        };
        assert_eq!(
            all,
            BTreeSet::from_iter((0..251).map(|n| format!("{prefix}{n:03}")))
        );
    }
    let sorted = a
        .page("/admin/usage?group=user&range=24h&sort=input&direction=desc&limit=3")
        .await;
    assert!(sorted.contains("<select name=\"sort\""));
    assert!(sorted.contains("<select name=\"direction\""));
    assert_eq!(
        usage_names(&sorted),
        vec!["User 250", "User 249", "User 248"]
    );
    let next = page_link(&sorted, "next").unwrap();
    for value in [
        "group=user",
        "range=24h",
        "sort=input",
        "direction=desc",
        "limit=3",
    ] {
        assert!(next.contains(value), "{next}");
    }
    for (path, marker) in [
        ("routes", "<section class=\"panel\" id=\"r-"),
        ("providers", "<section class=\"panel provider\" id=\"p-"),
    ] {
        let mut all = 0;
        let mut names = BTreeSet::new();
        let mut previous = None;
        let mut url = format!("/admin/{path}");
        loop {
            let html = a.page(&url).await;
            let n = html.matches(marker).count();
            assert!(n <= 50, "{path}: {n}");
            all += n;
            let current = section_names(&html, marker);
            for name in &current {
                assert!(names.insert(name.clone()), "duplicate {name}");
            }
            if let Some(expected) = previous {
                let prev = page_link(&html, "prev").expect("configuration Previous link");
                assert_eq!(section_names(&a.page(&prev).await, marker), expected);
            }
            match page_link(&html, "next") {
                Some(next) => {
                    previous = Some(current);
                    url = next;
                }
                None => break,
            }
        }
        assert_eq!(all, 251, "{path}");
        let prefix = path.strip_suffix('s').unwrap();
        assert_eq!(
            names,
            BTreeSet::from_iter((0..251).map(|n| format!("{prefix}-{n:03}")))
        );
        assert_eq!(
            a.page(&format!("/admin/{path}?limit=100000"))
                .await
                .matches(marker)
                .count(),
            200
        );
        assert_eq!(
            a.page(&format!("/admin/{path}?q=042"))
                .await
                .matches(marker)
                .count(),
            1
        );
    }
    // Switch through the supported form to exercise the real Simple Mode surface.
    let response = a
        .client
        .post(format!("{}/admin/settings/mode", a.url))
        .header("cookie", "tokomotive_session=pagination-session")
        .form(&[("mode", "simple"), ("confirm", "on")])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let html = a.page("/admin/usage?sort=price_micros").await;
    assert!(!html.contains("data-label=\"Price\"") && !html.contains(">Cost</a>"));
    assert_eq!(
        usage_names(&a.page("/admin/usage?limit=100000").await).len(),
        200
    );
}

#[tokio::test]
async fn nested_configuration_lists_are_paged_without_losing_hidden_rows() {
    let a = App::large().await;
    let html = a.page("/admin/routes?q=route-000").await;
    let form = html
        .split("aria-label=\"Edit Route route-000\"")
        .nth(1)
        .unwrap()
        .split("</form>")
        .next()
        .unwrap();
    assert_eq!(
        form.split("<noscript>")
            .next()
            .unwrap()
            .matches("name=\"deployment_model\"")
            .count(),
        50
    );
    assert!(form.contains("name=\"deployment_offset\" value=\"0\""));
    let second = a.page("/admin/routes?q=route-000&deployments_page=2").await;
    assert!(second.contains("name=\"deployment_offset\" value=\"50\""));
    let response = a
        .client
        .post(format!("{}/admin/routes/route-000", a.url))
        .header("cookie", "tokomotive_session=pagination-session")
        .form(&[
            ("name", "route-000"),
            ("strategy", "priority"),
            ("deployment_offset", "50"),
            ("deployment_count", "50"),
            ("deployment_provider", "provider-000"),
            ("deployment_model", "changed"),
            ("deployment_weight", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cfg = tokomotive::settings::load(&a.st.db, &a.st.secret_key)
        .await
        .unwrap()
        .0;
    assert_eq!(cfg.routes[0].deployments.len(), 72);
    assert_eq!(cfg.routes[0].deployments[50].model, "changed");
    assert_eq!(cfg.routes[0].deployments[51].model, "upstream");
    let first = a.page("/admin/providers?q=provider-000").await;
    assert_eq!(
        first
            .split("class=\"key-list\"")
            .nth(1)
            .unwrap()
            .split("</ul>")
            .next()
            .unwrap()
            .matches("<li>")
            .count(),
        50
    );
    let second = a.page("/admin/providers?q=provider-000&pool_page=2").await;
    assert_eq!(
        second
            .split("class=\"key-list\"")
            .nth(1)
            .unwrap()
            .split("</ul>")
            .next()
            .unwrap()
            .matches("<li>")
            .count(),
        50
    );
    assert!(second.contains("/keys/50/delete"));
    assert!(!second.contains("/keys/0/delete"));
}

fn section_names(html: &str, marker: &str) -> Vec<String> {
    html.split(marker)
        .skip(1)
        .map(|s| s.split('"').next().unwrap().to_owned())
        .collect()
}

fn usage_names(html: &str) -> Vec<String> {
    html.split("<tr><th scope=\"row\">")
        .skip(1)
        .map(|s| s.split("</th>").next().unwrap().to_owned())
        .collect()
}

async fn walk_usage(a: &App, group: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut url = format!("/admin/usage?group={group}&range=24h&sort=key&direction=asc");
    let mut previous = None;
    loop {
        let html = a.page(&url).await;
        let names = usage_names(&html);
        assert!(names.len() <= 50);
        for name in &names {
            assert!(found.insert(name.clone()));
        }
        if let Some(expected) = previous {
            let prev = page_link(&html, "prev").expect("Usage Previous link");
            assert_eq!(usage_names(&a.page(&prev).await), expected);
        }
        match page_link(&html, "next") {
            Some(next) => {
                previous = Some(names);
                url = next;
            }
            None => return found,
        }
    }
}

#[tokio::test]
async fn anthropic_model_cursors_and_openai_cap() {
    let a = App::large().await;
    let mut all = BTreeSet::new();
    let mut query = "?limit=37".to_owned();
    let mut previous: Option<Vec<Value>> = None;
    loop {
        let page = a.models(&query, true).await;
        let data = page["data"].as_array().unwrap();
        assert!(data.len() <= 37);
        assert_eq!(page["first_id"], data.first().unwrap()["id"]);
        assert_eq!(page["last_id"], data.last().unwrap()["id"]);
        if let Some(expected) = previous {
            let prev = a
                .models(
                    &format!("?limit=37&before_id={}", page["first_id"].as_str().unwrap()),
                    true,
                )
                .await;
            assert_eq!(prev["data"], Value::Array(expected));
        }
        for row in data {
            assert!(all.insert(row["id"].as_str().unwrap().to_owned()));
        }
        if page["has_more"] == false {
            break;
        }
        previous = Some(data.clone());
        query = format!("?limit=37&after_id={}", page["last_id"].as_str().unwrap());
    }
    assert_eq!(all.len(), 502);
    let empty = a.models("?after_id=zzzz", true).await;
    assert_eq!(empty["data"], serde_json::json!([]));
    assert_eq!(empty["first_id"], Value::Null);
    assert_eq!(empty["last_id"], Value::Null);
    assert_eq!(empty["has_more"], false);
    assert_eq!(
        a.models("?limit=100000", true).await["data"]
            .as_array()
            .unwrap()
            .len(),
        200
    );
    for n in 251..1005 {
        db::create_model(&a.st.db, &format!("model-{n:04}"), "provider-000", &[])
            .await
            .unwrap();
    }
    a.st.reload_catalog().await.unwrap();
    let capped = a.models("", false).await;
    assert_eq!(capped["object"], "list");
    assert_eq!(capped["data"].as_array().unwrap().len(), 1000);
    // A single Model lookup must still work beyond the list cap.
    let response = a
        .client
        .get(format!("{}/v1/models/route-250", a.url))
        .bearer_auth(&a.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn exact_name_inputs_work_without_javascript_and_reject_unknown_ids() {
    let a = App::large().await;
    for (path, form) in [
        (
            "/admin/keys",
            vec![("user_name", "User 250"), ("label", "no-js")],
        ),
        (
            "/admin/users",
            vec![("name", "No JS User"), ("team_name", "Team 250")],
        ),
        (
            "/admin/models/1/team-price",
            vec![
                ("team_name", "Team 250"),
                ("mode", "markup"),
                ("markup_percent", "10"),
            ],
        ),
    ] {
        let response = a
            .client
            .post(format!("{}{path}", a.url))
            .header("cookie", "tokomotive_session=pagination-session")
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
    let users = db::users_page(
        &a.st.db,
        &tokomotive::pagination::PageQuery {
            q: "No JS User".into(),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(users.items[0].team_id, Some(251));
    for (path, form) in [
        ("/admin/keys", vec![("user_id", "100000")]),
        (
            "/admin/users",
            vec![("name", "Invalid"), ("team_id", "Not a Team")],
        ),
        (
            "/admin/models/1/team-price",
            vec![
                ("team_id", "100000"),
                ("mode", "markup"),
                ("markup_percent", "10"),
            ],
        ),
    ] {
        let response = a
            .client
            .post(format!("{}{path}", a.url))
            .header("cookie", "tokomotive_session=pagination-session")
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{path}");
        assert!(response.text().await.unwrap().contains("matches this name"));
    }
    for kind in ["users", "teams", "models", "providers", "routes"] {
        let data: Value =
            serde_json::from_str(&a.page(&format!("/admin/search/{kind}?q=")).await).unwrap();
        assert!(data.as_array().unwrap().len() <= 20);
    }
}

#[tokio::test]
async fn paged_query_plans_use_ledger_indexes() {
    use sqlx::Row;
    let a = App::large().await;
    let queries = [
        (
            "Users",
            "SELECT u.id,u.name,(SELECT COUNT(*) FROM virtual_keys k WHERE k.user_id=u.id AND k.revoked_at IS NULL) FROM users u WHERE u.id>50 AND instr(lower(u.name),lower('User'))>0 ORDER BY u.id LIMIT 51",
        ),
        (
            "Team members",
            "SELECT id,name FROM users WHERE team_id=1 AND id>0 ORDER BY id LIMIT 51",
        ),
        (
            "Teams",
            "SELECT t.id,t.name,(SELECT COUNT(*) FROM users u WHERE u.team_id=t.id) FROM teams t WHERE t.id>50 ORDER BY t.id LIMIT 51",
        ),
        (
            "Active Keys",
            "SELECT k.id,k.label,u.name FROM virtual_keys k JOIN users u ON u.id=k.user_id WHERE k.revoked_at IS NULL AND k.id>50 ORDER BY k.id LIMIT 51",
        ),
        (
            "Revoked Keys",
            "SELECT k.id,k.label,u.name FROM virtual_keys k JOIN users u ON u.id=k.user_id WHERE k.revoked_at IS NOT NULL AND k.id>50 ORDER BY k.id LIMIT 51",
        ),
        (
            "User Keys",
            "SELECT k.id,k.label,u.name FROM virtual_keys k JOIN users u ON u.id=k.user_id WHERE k.revoked_at IS NULL AND k.user_id=1 AND k.id>0 ORDER BY k.id LIMIT 51",
        ),
        (
            "Models",
            "SELECT id FROM models WHERE id>50 AND instr(lower(name),lower('model'))>0 ORDER BY id LIMIT 51",
        ),
        (
            "History",
            "SELECT c.id,t.name,c.spec FROM cards c LEFT JOIN teams t ON t.id=c.team_id WHERE c.model_id=1 AND c.id<300 ORDER BY c.id DESC LIMIT 21",
        ),
        (
            "Overrides",
            "SELECT t.id,t.name FROM teams t WHERE t.id>0 AND EXISTS(SELECT 1 FROM cards c WHERE c.team_id=t.id AND c.model_id=1 AND c.kind='price' AND c.spec!='null' AND c.id=(SELECT MAX(c2.id) FROM cards c2 WHERE c2.team_id=t.id AND c2.model_id=c.model_id AND c2.kind='price')) ORDER BY t.id LIMIT 21",
        ),
        (
            "Routes",
            "SELECT config_json FROM routes WHERE instr(lower(name),lower('route'))>0 ORDER BY name LIMIT 51 OFFSET 50",
        ),
        (
            "Providers",
            "SELECT name,kind,base_url,auth FROM providers WHERE instr(lower(name),lower('provider'))>0 ORDER BY name LIMIT 51 OFFSET 50",
        ),
        (
            "Suggestions",
            "SELECT id,name FROM users WHERE instr(lower(name),lower('User'))>0 ORDER BY name LIMIT 20",
        ),
        (
            "Team suggestions",
            "SELECT id,name FROM teams WHERE instr(lower(name),lower('Team'))>0 ORDER BY name LIMIT 20",
        ),
        (
            "Model suggestions",
            "SELECT id,name FROM models WHERE instr(lower(name),lower('model'))>0 ORDER BY name LIMIT 20",
        ),
        (
            "Route suggestions",
            "SELECT rowid,name FROM routes WHERE instr(lower(name),lower('route'))>0 ORDER BY name LIMIT 20",
        ),
        (
            "Provider suggestions",
            "SELECT rowid,name FROM providers WHERE instr(lower(name),lower('provider'))>0 ORDER BY name LIMIT 20",
        ),
        (
            "Resolve User",
            "SELECT id FROM users WHERE name='User 250' OR id=NULL ORDER BY name='User 250' DESC LIMIT 1",
        ),
        (
            "Resolve Team",
            "SELECT id FROM teams WHERE name='Team 250' OR id=NULL ORDER BY name='Team 250' DESC LIMIT 1",
        ),
        (
            "Team Models",
            "SELECT m.id FROM models m WHERE m.id>0 AND EXISTS(SELECT 1 FROM cards c WHERE c.model_id=m.id AND c.team_id=1 AND c.kind='price' AND c.spec!='null' AND c.id=(SELECT MAX(c2.id) FROM cards c2 WHERE c2.model_id=c.model_id AND c2.team_id=c.team_id AND c2.kind='price')) ORDER BY m.id LIMIT 51",
        ),
        (
            "Route metadata",
            "SELECT json_set(config_json,'$.deployments',json('[]')) FROM routes ORDER BY name LIMIT 51 OFFSET 50",
        ),
        (
            "Deployments",
            "SELECT j.value FROM routes r,json_each(r.config_json,'$.deployments') j WHERE r.name='route-000' ORDER BY CAST(j.key AS INTEGER) LIMIT 51 OFFSET 50",
        ),
        (
            "Provider Key Pool",
            "SELECT CAST(j.key AS INTEGER),j.value FROM providers p,json_each(p.keys_json) j WHERE p.name='provider-000' ORDER BY CAST(j.key AS INTEGER) LIMIT 51 OFFSET 50",
        ),
        (
            "Ledger Key labels",
            "SELECT j.value FROM providers p,json_each(p.keys_json) j WHERE p.name='provider-000' ORDER BY CAST(j.key AS INTEGER) LIMIT 200 OFFSET 0",
        ),
        (
            "Reprice batch",
            "SELECT id,ts_ms,user_id,input,output FROM ledger WHERE unpriced=1 AND id>0 ORDER BY id LIMIT 200",
        ),
        (
            "Catalog batch",
            "SELECT id,name,provider,aliases FROM models WHERE id>0 ORDER BY id LIMIT 200",
        ),
        (
            "Catalog Cards batch",
            "SELECT c.id,c.model_id,c.team_id,c.kind,c.spec FROM cards c WHERE c.id>0 AND c.id=(SELECT MAX(c2.id) FROM cards c2 WHERE c2.model_id=c.model_id AND c2.team_id IS c.team_id AND c2.kind=c.kind) ORDER BY c.id LIMIT 200",
        ),
        (
            "Runtime Providers batch",
            "SELECT * FROM providers WHERE name>'provider-100' ORDER BY name LIMIT 200",
        ),
        (
            "Runtime Routes batch",
            "SELECT name,config_json FROM routes WHERE name>'route-100' ORDER BY name LIMIT 200",
        ),
        (
            "Seed Teams batch",
            "SELECT id,name FROM teams WHERE id>0 ORDER BY id LIMIT 200",
        ),
        (
            "Ledger first page",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.id>0 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger cursor",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger User",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.user_id=1 AND l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger Provider",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.provider='provider-000' AND l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger Model",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE (l.model='model-000' OR l.requested_model='model-000') ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger Unpriced",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.unpriced=1 AND l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger Estimated",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.estimated=1 AND l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger Errors",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.status>=400 AND l.id<900 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Ledger dates",
            "SELECT l.*,u.name FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.ts_ms>=0 AND l.ts_ms<9999999999999 ORDER BY l.id DESC LIMIT 101",
        ),
        (
            "Usage Model",
            "SELECT COALESCE(m.name,l.model,l.requested_model,'') AS key,COUNT(*) AS requests FROM ledger l JOIN users u ON u.id=l.user_id LEFT JOIN teams t ON t.id=u.team_id LEFT JOIN models m ON m.id=l.model_id WHERE l.ts_ms>=1 GROUP BY key ORDER BY requests DESC,key LIMIT 51 OFFSET 50",
        ),
        (
            "Usage User",
            "SELECT u.name AS key,COUNT(*) AS requests FROM ledger l JOIN users u ON u.id=l.user_id WHERE l.ts_ms>=1 GROUP BY u.id ORDER BY requests DESC,key LIMIT 51 OFFSET 50",
        ),
        (
            "Usage Team",
            "SELECT IFNULL(t.name,'(no team)') AS key,COUNT(*) AS requests FROM ledger l JOIN users u ON u.id=l.user_id LEFT JOIN teams t ON t.id=u.team_id WHERE l.ts_ms>=1 GROUP BY u.team_id ORDER BY requests DESC,key LIMIT 51 OFFSET 50",
        ),
        (
            "Usage Provider",
            "SELECT l.provider AS key,COUNT(*) AS requests FROM ledger l INDEXED BY ledger_ts WHERE l.ts_ms>=1 GROUP BY l.provider ORDER BY requests DESC,key LIMIT 51 OFFSET 50",
        ),
    ];
    for (name, sql) in queries {
        let rows = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}"))
            .fetch_all(&a.st.db)
            .await
            .unwrap();
        let details: Vec<String> = rows.iter().map(|row| row.get("detail")).collect();
        println!("{name}: {}", details.join("; "));
        if name.starts_with("Ledger") || name.starts_with("Usage") {
            assert!(
                !details
                    .iter()
                    .any(|s| s == "SCAN l" || s.starts_with("SCAN l ")),
                "{name}: {details:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_numeric_name_does_not_select_another_users_id() {
    let a = App::large().await;
    let html = a.page("/admin/keys").await;
    let input = html
        .split("id=\"key-user\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    let field = input
        .split("name=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    // ID 251 exists, but no User has the name "251".
    let response = a
        .client
        .post(format!("{}/admin/keys", a.url))
        .header("cookie", "tokomotive_session=pagination-session")
        .form(&[(field, "251")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("No User matches this name")
    );
}

#[tokio::test]
async fn reprice_batches_preserve_atomicity_and_rollup_totals() {
    let a = App::large().await;
    sqlx::query("UPDATE ledger SET unpriced=1")
        .execute(&a.st.db)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_reprice BEFORE UPDATE ON ledger WHEN OLD.id=401 AND NEW.unpriced=0 BEGIN SELECT RAISE(ABORT,'fixture failure'); END")
        .execute(&a.st.db).await.unwrap();
    assert!(
        db::reprice_unpriced(&a.st.db, &a.st.catalog())
            .await
            .is_err()
    );
    assert_eq!(db::count_flags(&a.st.db).await.unwrap().0, 1025);
    sqlx::query("DROP TRIGGER fail_reprice")
        .execute(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        db::reprice_unpriced(&a.st.db, &a.st.catalog())
            .await
            .unwrap(),
        1025
    );
    assert_eq!(db::count_flags(&a.st.db).await.unwrap().0, 0);
    assert_eq!(db::totals_since(&a.st.db, 0).await.unwrap().requests, 1025);
}

#[tokio::test]
#[ignore = "Run manually for Playwright; serves the large fixture until stopped."]
async fn visual_fixture() {
    let app = if std::env::var_os("PAGINATION_SMALL").is_some() {
        App::small().await
    } else {
        App::large().await
    };
    println!("PAGINATION_URL={}", app.url);
    std::future::pending::<()>().await;
}

async fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    (dir, tokomotive::init(&cfg, None).await.unwrap())
}

#[tokio::test]
async fn legacy_list_is_bounded() {
    let (_dir, st) = state().await;
    for n in 0..251 {
        db::create_user(&st.db, &format!("User {n:03}"), None, None, false)
            .await
            .unwrap();
    }
    assert!(db::list_users(&st.db).await.unwrap().len() <= 200);
}

/// A list that fits on one page has no pager: "Showing N" alone is noise.
#[tokio::test]
async fn single_page_lists_render_no_pager() {
    let a = App::large().await;
    // One matching User: one page, no Previous and no Next.
    let html = a.page("/admin/users?q=User%20001").await;
    assert!(
        !html.contains("<nav class=\"pager\""),
        "pager on a one-page list"
    );
    // A full page with a next page keeps the pager.
    let html = a.page("/admin/users?limit=10").await;
    assert!(html.contains("<nav class=\"pager\""));
    assert!(html.contains("rel=\"next\""));
}
