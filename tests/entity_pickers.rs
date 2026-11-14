//! Entity pickers (issue #12): every field that names a User, Team,
//! Provider, Route or models.dev entry is a searchable list, never a field
//! that needs the exact name typed.
use serde_json::Value;
use tokomotive::{config::Config, db, state::AppState};

const SESSION: &str = "tokomotive_session=picker-session";

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
}

/// 25 Users (User 00 has one active key and Team Alpha), 3 Teams,
/// 2 Providers (one subscription) and 2 Routes.
async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let text = "\
[[providers]]\nname='openai'\nkind='openai'\nbase_url='http://127.0.0.1:9'\napi_key='k'\n\
[[providers]]\nname='claude-api'\nkind='anthropic'\nbase_url='http://127.0.0.1:9'\napi_key='k'\n\
[[routes]]\nname='fast'\nstrategy='round_robin'\n[[routes.deployments]]\nprovider='openai'\nmodel='a'\n[[routes.deployments]]\nprovider='openai'\nmodel='b'\n\
[[routes]]\nname='smart'\n[[routes.deployments]]\nprovider='claude-api'\nmodel='c'\n";
    let mut cfg: Config = toml::from_str(text).unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::settings::put(&st.db, "admin_password_hash", "fixture-password-hash")
        .await
        .unwrap();
    db::create_session(&st.db, "picker-session", 1)
        .await
        .unwrap();
    let alpha = db::create_team(&st.db, "Alpha").await.unwrap();
    db::create_team(&st.db, "Beta").await.unwrap();
    db::create_team(&st.db, "Gamma").await.unwrap();
    for n in 0..25 {
        let team = (n < 2).then_some(alpha);
        let u = db::create_user(&st.db, &format!("User {n:02}"), team, None, false)
            .await
            .unwrap();
        if n == 0 {
            db::create_key(&st.db, u, "laptop", None).await.unwrap();
        }
    }
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

impl App {
    async fn get(&self, path: &str, accept: &str) -> reqwest::Response {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", SESSION)
            .header("accept", accept)
            .send()
            .await
            .unwrap()
    }
    async fn json(&self, path: &str) -> (Value, Option<i64>) {
        let r = self.get(path, "application/json").await;
        assert_eq!(r.status(), 200, "{path}");
        let total = r
            .headers()
            .get("x-total-count")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        (r.json().await.unwrap(), total)
    }
    async fn html(&self, path: &str) -> String {
        let r = self.get(path, "text/html").await;
        assert_eq!(r.status(), 200, "{path}");
        r.text().await.unwrap()
    }
}

const KINDS: [&str; 6] = [
    "users",
    "teams",
    "providers",
    "routes",
    "models-dev-providers",
    "models-dev-models",
];

#[tokio::test]
async fn search_endpoints_are_admin_only() {
    let a = app().await;
    for kind in KINDS {
        let r =
            a.c.get(format!("{}/admin/search/{kind}?q=", a.url))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 303, "{kind}");
        assert_eq!(r.headers()["location"], "/admin/login", "{kind}");
    }
    let r = a.get("/admin/search/nothing?q=", "text/html").await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn empty_query_lists_the_first_twenty_with_the_total() {
    let a = app().await;
    for kind in KINDS {
        let (rows, total) = a.json(&format!("/admin/search/{kind}?q=")).await;
        let rows = rows.as_array().unwrap();
        assert!(!rows.is_empty(), "{kind}: empty query lists items");
        assert!(rows.len() <= 20, "{kind}: at most 20");
        assert!(total.unwrap() >= rows.len() as i64, "{kind}: total header");
    }
    let (users, total) = a.json("/admin/search/users?q=").await;
    let names: Vec<&str> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 20);
    assert_eq!(total, Some(25));
    assert_eq!(names[0], "User 00", "ordered by name");
    assert_eq!(names[19], "User 19");
    let (teams, total) = a.json("/admin/search/teams?q=").await;
    assert_eq!(total, Some(3));
    assert_eq!(teams[0]["name"], "Alpha");
}

#[tokio::test]
async fn typed_text_filters_and_names_that_start_with_it_come_first() {
    let a = app().await;
    let (users, total) = a.json("/admin/search/users?q=user%202").await;
    let names: Vec<&str> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["User 20", "User 21", "User 22", "User 23", "User 24"]
    );
    assert_eq!(total, Some(5));
    // A User is also found by the name of its Team.
    let (users, _) = a.json("/admin/search/users?q=alpha").await;
    assert_eq!(users.as_array().unwrap().len(), 2);
    let (providers, _) = a.json("/admin/search/providers?q=AI").await;
    assert_eq!(providers.as_array().unwrap().len(), 1);
    assert_eq!(providers[0]["name"], "openai");
    let (none, total) = a.json("/admin/search/teams?q=zzz").await;
    assert!(none.as_array().unwrap().is_empty());
    assert_eq!(total, Some(0));
}

#[tokio::test]
async fn options_show_context() {
    let a = app().await;
    let (users, _) = a.json("/admin/search/users?q=User%2000").await;
    assert_eq!(users[0]["detail"], "Team Alpha, 1 key");
    let (users, _) = a.json("/admin/search/users?q=User%2010").await;
    assert_eq!(users[0]["detail"], "No Team, 0 keys");
    let (teams, _) = a.json("/admin/search/teams?q=Alpha").await;
    assert_eq!(teams[0]["detail"], "2 Users");
    let (providers, _) = a.json("/admin/search/providers?q=claude").await;
    assert_eq!(providers[0]["detail"], "Anthropic format, API key");
    let (routes, _) = a.json("/admin/search/routes?q=").await;
    assert_eq!(routes[0]["name"], "fast");
    assert_eq!(routes[0]["detail"], "Round robin, 2 Deployments");
    assert_eq!(routes[1]["detail"], "Priority, 1 Deployment");
    let (dev, _) = a.json("/admin/search/models-dev-providers?q=openai").await;
    assert_eq!(dev[0]["name"], "openai");
    let (dev, _) = a
        .json("/admin/search/models-dev-models?provider=openai&q=gpt-4o")
        .await;
    let dev = dev.as_array().unwrap();
    assert!(dev.iter().any(|m| m["name"] == "gpt-4o"), "{dev:?}");
    assert!(dev.iter().all(|m| m["scope"] == "openai"));

    // HTML options for the combobox.
    let html = a.html("/admin/search/users?q=User%2000").await;
    assert!(
        html.contains(
            "<li role=\"option\" aria-selected=\"false\" class=\"cb-opt\" data-value=\"User 00\">"
        ),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"cb-meta\">Team Alpha, 1 key</span>"),
        "{html}"
    );
    let html = a.html("/admin/search/models-dev-models?q=gpt-4o").await;
    assert!(
        html.contains("data-scope-value=\""),
        "unscoped models fill the provider: {html}"
    );
    // Text is escaped.
    db::create_team(&a.st.db, "<b>x</b>").await.unwrap();
    let html = a.html("/admin/search/teams?q=%3Cb").await;
    assert!(
        html.contains("&#60;b&#62;x") && !html.contains("<b>"),
        "{html}"
    );
}

#[tokio::test]
async fn keys_and_ledger_render_a_user_picker() {
    let a = app().await;
    let keys = a.html("/admin/keys").await;
    assert!(
        keys.contains("data-kind=\"users\" data-endpoint=\"/admin/search/users\""),
        "{keys}"
    );
    // 25 Users: without JS, a select lists them all.
    assert!(
        keys.contains("<select id=\"key-user\" name=\"user_name\" required>"),
        "{keys}"
    );
    let ledger = a.html("/admin/ledger").await;
    assert!(
        ledger.contains("<select id=\"lf-user\" name=\"user\">"),
        "{ledger}"
    );
    for n in 25..31 {
        db::create_user(&a.st.db, &format!("User {n:02}"), None, None, false)
            .await
            .unwrap();
    }
    // More than 30: a text input that JS makes a picker.
    let keys = a.html("/admin/keys").await;
    assert!(
        keys.contains("<input id=\"key-user\" name=\"user_name\""),
        "{keys}"
    );
    assert!(!keys.contains("data-search="), "no datalist search");
    let ledger = a.html("/admin/ledger").await;
    assert!(
        ledger.contains("<input id=\"lf-user\" name=\"user\""),
        "{ledger}"
    );
}

#[tokio::test]
async fn models_dev_overrides_are_pickers() {
    let a = app().await;
    let model = db::create_model(&a.st.db, "fixture-model", "openai", &[])
        .await
        .unwrap();
    a.st.reload_catalog().await.unwrap();
    let html = a.html(&format!("/admin/models/{model}")).await;
    assert!(
        html.contains("data-endpoint=\"/admin/search/models-dev-providers\""),
        "{html}"
    );
    assert!(
        html.contains(
            "data-endpoint=\"/admin/search/models-dev-models\" data-scope=\"models_dev_provider\""
        ),
        "{html}"
    );
}

// ------------------------------------------------------------ guard

/// The `name` of every literal `<input ...>` tag in a template, with the
/// tag text and the text before it.
fn inputs(html: &str) -> Vec<(String, String, String)> {
    let mut out = vec![];
    let mut at = 0;
    while let Some(i) = html[at..].find("<input") {
        let start = at + i;
        // The tag ends at the first '>' outside {{ }} and {% %}.
        let bytes = html.as_bytes();
        let mut j = start;
        let mut depth = 0;
        while j < bytes.len() {
            if html[j..].starts_with("{{") || html[j..].starts_with("{%") {
                depth += 1;
                j += 2;
                continue;
            }
            if html[j..].starts_with("}}") || html[j..].starts_with("%}") {
                depth -= 1;
                j += 2;
                continue;
            }
            if bytes[j] == b'>' && depth == 0 {
                break;
            }
            j += 1;
        }
        let tag = &html[start..=j.min(bytes.len() - 1)];
        let name = tag
            .split("name=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or("")
            .to_owned();
        out.push((name, tag.to_owned(), html[..start].to_owned()));
        at = j;
    }
    out
}

fn identifier_like(name: &str) -> bool {
    matches!(
        name,
        "user" | "users" | "team" | "teams" | "provider" | "providers" | "route" | "routes"
    ) || name.ends_with("_name")
        || name.ends_with("_user")
        || name.ends_with("_team")
        || name.ends_with("_provider")
        || name.ends_with("_route")
        || name.starts_with("allowed_")
        || name.starts_with("models_dev_")
}

/// A merge or a new form must not bring back a field that needs a typed
/// User, Team, Provider, Route or models.dev name. Use the pickers in
/// picker.html (base.html items 12 and 13).
#[test]
fn identifier_fields_are_pickers() {
    let mut bad = vec![];
    for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/templates")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let html = std::fs::read_to_string(&path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !html.contains("data-search"),
            "{file}: data-search datalist is retired"
        );
        for (name, tag, before) in inputs(&html) {
            if name.contains("{{") || !identifier_like(&name) {
                continue;
            }
            let kind = tag
                .split("type=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap_or("text");
            if matches!(kind, "hidden" | "checkbox" | "radio") {
                continue;
            }
            // A picker input is the first child of `<span class="cb" data-picker ...>`.
            let open = before.rfind('<').map(|i| &before[i..]).unwrap_or("");
            if open.starts_with("<span class=\"cb\"")
                && open.contains("data-picker")
                && before.trim_end().ends_with('>')
            {
                continue;
            }
            bad.push(format!("{file}: {tag}"));
        }
    }
    assert!(
        bad.is_empty(),
        "fields that need a typed name:\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_guard_finds_a_typed_name_field() {
    let html = r#"<label>User <input name="user_name" value="{{ u }}"></label>
<span class="cb" data-picker="single" data-strict><input name="team_name"></span>
<input type="hidden" name="provider" value="x"><input name="label">"#;
    let found: Vec<String> = inputs(html).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(found, ["user_name", "team_name", "provider", "label"]);
    assert!(identifier_like("user_name") && identifier_like("allowed_providers"));
    assert!(identifier_like("models_dev_model") && !identifier_like("name"));
    assert!(!identifier_like("label"));
}

#[test]
fn the_datalist_path_is_retired() {
    let base = include_str!("../templates/base.html");
    assert!(!base.contains("createElement('datalist')"));
    assert!(!base.contains("input[data-search]"));
}
