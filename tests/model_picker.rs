//! Model picker: the model search endpoint, its sources and the multi-value
//! form fields.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
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

/// A Provider model list on `/v1/models`. Counts the requests and waits
/// `delay` before it answers.
async fn upstream(ids: Vec<String>, delay: Duration) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let data: Vec<Value> = ids
        .iter()
        .map(|id| {
            if id == "gpt-4o" {
                serde_json::json!({"id": id, "context_length": 128000})
            } else {
                serde_json::json!({"id": id})
            }
        })
        .collect();
    let body = serde_json::json!({ "data": data });
    let router = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            async move {
                tokio::time::sleep(delay).await;
                axum::Json(body)
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, hits)
}

impl App {
    async fn get(&self, path: &str, accept: &str) -> reqwest::Response {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("accept", accept)
            .send()
            .await
            .unwrap()
    }

    async fn json(&self, path: &str) -> Vec<Value> {
        let r = self.get(path, "application/json").await;
        assert_eq!(r.status(), 200, "{path}");
        let v: Value = r.json().await.unwrap();
        v.as_array().unwrap().clone()
    }

    async fn page(&self, path: &str) -> String {
        self.get(path, "text/html").await.text().await.unwrap()
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

    async fn provider(&self, name: &str, base: &str, auth: &str) {
        let mut form = vec![
            ("name", name),
            ("kind", "openai"),
            ("base_url", base),
            ("api_key", "sk-test-1234"),
        ];
        if auth != "api_key" {
            form = vec![
                ("name", name),
                ("kind", "anthropic"),
                ("auth", auth),
                ("base_url", base),
            ];
        }
        let r = self.post("/admin/providers", &form).await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    }

    async fn model(&self, name: &str, provider: &str, aliases: &str) {
        let r = self
            .post(
                "/admin/models",
                &[("name", name), ("provider", provider), ("aliases", aliases)],
            )
            .await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    }

    async fn route(&self, form: &[(&str, &str)]) -> reqwest::Response {
        self.post("/admin/routes", form).await
    }
}

fn ids(hits: &[Value]) -> Vec<&str> {
    hits.iter().map(|h| h["id"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn unconfigured_providers_have_no_models_dev_hits() {
    let a = app().await;
    let hits = a.json("/admin/search/models?q=gpt&src=models.dev").await;
    assert!(hits.is_empty(), "unconfigured models: {hits:?}");
}

#[tokio::test]
async fn connected_models_dev_names_use_configured_provider_and_merge_live_metadata() {
    let a = app().await;
    let (base, _) = upstream(vec!["gpt-4o".into()], Duration::ZERO).await;
    a.provider("work", &base, "api_key").await;
    sqlx::query("UPDATE providers SET preset='openai' WHERE name='work'")
        .execute(&a.st.db)
        .await
        .unwrap();
    // Independent fixture: catalog metadata must replace the live context.
    sqlx::query(
        "UPDATE models_dev_models SET context=777000 WHERE provider='openai' AND id='gpt-4o'",
    )
    .execute(&a.st.db)
    .await
    .unwrap();
    let dev = a.json("/admin/search/models?q=gpt-4o&src=models.dev").await;
    assert!(ids(&dev).contains(&"work/gpt-4o"), "{dev:?}");
    assert!(
        dev.iter()
            .all(|h| h["provider"] == "work" && h["id"].as_str().unwrap().starts_with("work/"))
    );
    let hits = a.json("/admin/search/models?q=gpt-4o").await;
    let matching: Vec<_> = hits.iter().filter(|h| h["id"] == "work/gpt-4o").collect();
    assert_eq!(matching.len(), 1, "{hits:?}");
    assert_eq!(matching[0]["source"], "provider");
    assert_eq!(matching[0]["context"], 777000);
    assert!(matching[0]["price"].is_string());
    let qualified = a
        .json("/admin/search/models?q=work/gpt-4o&src=models.dev")
        .await;
    assert_eq!(ids(&qualified)[0], "work/gpt-4o");
}

#[tokio::test]
async fn models_dev_ranks_before_applying_the_source_limit() {
    let a = app().await;
    let (base, _) = upstream(vec![], Duration::ZERO).await;
    a.provider("work", &base, "api_key").await;
    sqlx::query("UPDATE providers SET preset='openai' WHERE name='work'")
        .execute(&a.st.db)
        .await
        .unwrap();
    for i in 0..60 {
        sqlx::query("INSERT INTO models_dev_models(provider,id,name,canonical_model_id,details) VALUES('openai',?,?, '', '{}')")
            .bind(format!("my-picker17-{i}"))
            .bind(format!("AAA picker17 {i}"))
            .execute(&a.st.db).await.unwrap();
    }
    sqlx::query("INSERT INTO models_dev_models(provider,id,name,canonical_model_id,details) VALUES('openai','picker17','ZZZ', '', '{}')").execute(&a.st.db).await.unwrap();
    let hits = a
        .json("/admin/search/models?q=picker17&provider=work&src=models.dev")
        .await;
    assert_eq!(hits.len(), 20);
    assert_eq!(ids(&hits)[0], "picker17");
}

#[tokio::test]
async fn disconnected_signin_and_missing_env_key_exclude_provider_models() {
    let a = app().await;
    a.provider("signed-out", "https://api.anthropic.com", "claude")
        .await;
    let (base, requests) = upstream(vec!["gpt-live".into()], Duration::ZERO).await;
    a.provider("missing-key", &base, "api_key").await;
    sqlx::query("UPDATE providers SET keys_json=? WHERE name='missing-key'")
        .bind(r#"["env:MODEL_PICKER_ABSENT_KEY_17"]"#)
        .execute(&a.st.db)
        .await
        .unwrap();
    a.st.reload_settings().await.unwrap();
    // A stale runtime key must not make a missing env reference connected.
    let mut table = (*a.st.router_table()).clone();
    table
        .providers
        .iter_mut()
        .find(|p| p.name == "missing-key")
        .unwrap()
        .keys = Arc::new(vec!["stale-key".into()]);
    a.st.set_router_table(table);
    a.model("gpt-catalog", "signed-out", "").await;
    assert!(
        a.json("/admin/search/models?provider=signed-out")
            .await
            .is_empty()
    );
    assert!(
        a.json("/admin/search/models?provider=missing-key")
            .await
            .is_empty()
    );
    assert!(
        a.json("/admin/search/models?q=gpt&src=provider,models.dev")
            .await
            .is_empty()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    // Unscoped catalog Models remain available.
    assert_eq!(
        ids(&a.json("/admin/search/models?q=gpt&src=model").await),
        ["gpt-catalog"]
    );
}

#[tokio::test]
async fn disconnected_scope_and_excluded_count_are_nonselectable_notes() {
    let a = app().await;
    a.provider("signed-out", "https://api.anthropic.com", "claude")
        .await;
    a.provider("also-out", "https://api.anthropic.com", "claude")
        .await;
    let html = a.page("/admin/search/models?provider=signed-out").await;
    assert!(
        html.contains("data-empty-note=\"This Provider is not connected. Sign in or add a key.\""),
        "{html}"
    );
    assert!(!html.contains("role=\"option\""));
    let html = a.page("/admin/search/models?src=route,model").await;
    assert!(
        html.contains("data-empty-note=\"2 Providers are not connected\""),
        "{html}"
    );
    assert!(!html.contains("role=\"option\""));
    let page = a.page("/admin/routes").await;
    assert!(
        page.contains("t.content.querySelector('[data-empty-note]')"),
        "picker must read response notes"
    );
}

#[tokio::test]
async fn search_ranks_dedupes_caps_and_scopes_by_provider() {
    let a = app().await;
    let mut listed: Vec<String> = (0..80).map(|i| format!("m-{i:02}")).collect();
    listed.extend(["gpt-4o".into(), "my-gpt-4o-copy".into()]);
    let (base, _) = upstream(listed, Duration::ZERO).await;
    a.provider("openai", &base, "api_key").await;
    let (other, _) = upstream(vec!["llama-3".into()], Duration::ZERO).await;
    a.provider("groq", &other, "api_key").await;
    a.model("gpt-4o", "openai", "4o").await;
    a.model("gpt-4o-mini", "openai", "").await;
    a.model("llama", "groq", "").await;
    let r = a
        .route(&[
            ("name", "gpt-route"),
            ("strategy", "priority"),
            ("fallbacks", ""),
            ("deployment_provider", "openai"),
            ("deployment_model", "gpt-4o"),
            ("deployment_weight", "1"),
        ])
        .await;
    assert_eq!(r.status(), 303);

    // Scoped: only hits of this Provider, exact first, one hit per id.
    let hits = a
        .json("/admin/search/models?q=gpt-4o&provider=openai")
        .await;
    let got = ids(&hits);
    assert_eq!(got[0], "gpt-4o", "{got:?}");
    assert_eq!(got[1], "gpt-4o-mini", "prefix before contains: {got:?}");
    assert!(got.contains(&"my-gpt-4o-copy"));
    assert_eq!(got.iter().filter(|i| **i == "gpt-4o").count(), 1);
    assert!(!got.contains(&"gpt-route"), "no Routes in a Provider scope");
    assert!(!got.contains(&"llama"));
    assert!(hits.iter().all(|h| h["provider"] == "openai"), "{hits:?}");
    assert_eq!(hits[0]["source"], "model", "the catalog Model is kept");
    assert_eq!(hits[0]["context"], 128000, "context from the live list");

    // Caps: 20 by default, at most 50.
    assert_eq!(
        a.json("/admin/search/models?q=m-&provider=openai")
            .await
            .len(),
        20
    );
    assert_eq!(
        a.json("/admin/search/models?q=m-&provider=openai&limit=50")
            .await
            .len(),
        50
    );
    assert_eq!(
        a.json("/admin/search/models?q=m-&provider=openai&limit=500")
            .await
            .len(),
        50
    );

    // Unscoped: Routes, Models and provider/model.
    let hits = a.json("/admin/search/models?q=gpt&limit=50").await;
    let got = ids(&hits);
    assert!(got.contains(&"gpt-route"), "{got:?}");
    assert!(got.contains(&"openai/gpt-4o"), "{got:?}");
    let route = hits.iter().find(|h| h["id"] == "gpt-route").unwrap();
    assert_eq!(route["source"], "route");

    // The src filter limits the sources.
    let hits = a
        .json("/admin/search/models?q=gpt&src=route,model&limit=50")
        .await;
    assert!(
        hits.iter()
            .all(|h| h["source"] == "route" || h["source"] == "model"),
        "{hits:?}"
    );
    // Aliases are hits too.
    let hits = a.json("/admin/search/models?q=4o&src=model").await;
    assert_eq!(ids(&hits)[0], "4o");
}

#[tokio::test]
async fn live_source_caches_and_a_slow_provider_does_not_fail_the_search() {
    let a = app().await;
    let (fast, fast_hits) = upstream(vec!["gpt-4o".into()], Duration::ZERO).await;
    a.provider("openai", &fast, "api_key").await;
    for _ in 0..3 {
        let hits = a.json("/admin/search/models?q=gpt&provider=openai").await;
        assert_eq!(ids(&hits), ["gpt-4o"]);
    }
    assert_eq!(fast_hits.load(Ordering::SeqCst), 1, "cached per Provider");

    let (slow, slow_hits) = upstream(vec!["slow-live".into()], Duration::from_secs(10)).await;
    a.provider("slow", &slow, "api_key").await;
    a.model("slow-model", "slow", "").await;
    let start = Instant::now();
    let hits = a.json("/admin/search/models?q=slow&provider=slow").await;
    let waited = start.elapsed();
    assert!(waited < Duration::from_millis(4500), "waited {waited:?}");
    assert_eq!(ids(&hits), ["slow-model"], "other sources still answer");
    // The failure is remembered, so the next search does not wait again.
    let start = Instant::now();
    a.json("/admin/search/models?q=slow&provider=slow").await;
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(slow_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn subscription_providers_are_never_queried() {
    let a = app().await;
    let (base, hits) = upstream(vec!["claude-live".into()], Duration::ZERO).await;
    a.provider("claude-max", "https://api.anthropic.com", "claude")
        .await;
    a.model("claude-sonnet", "claude-max", "").await;
    // Point the subscription at the mock and give it a key: it must still
    // not be queried.
    let mut table = (*a.st.router_table()).clone();
    for p in &mut table.providers {
        p.base_url = base.clone();
        p.keys = Arc::new(vec!["token".into()]);
    }
    a.st.set_router_table(table);
    let scoped = a.json("/admin/search/models?q=&provider=claude-max").await;
    // A signed-out Provider has no scoped suggestions, including catalog Models.
    assert!(scoped.is_empty());
    assert!(
        scoped.iter().all(|h| h["source"] != "provider"),
        "{scoped:?}"
    );
    assert!(!ids(&scoped).contains(&"claude-live"));
    a.json("/admin/search/models?q=claude").await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fragment_is_a_list_of_escaped_options() {
    let a = app().await;
    let (base, _) = upstream(vec!["gpt-4o".into(), "<b>x</b>".into()], Duration::ZERO).await;
    a.provider("openai", &base, "api_key").await;
    a.model("gpt-4o", "openai", "").await;
    let r = a
        .get("/admin/search/models?q=&provider=openai", "text/html")
        .await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = r.text().await.unwrap();
    let items = html.matches("<li ").count();
    assert_eq!(items, 2, "{html}");
    assert_eq!(html.matches("role=\"option\"").count(), items);
    assert_eq!(html.matches("aria-selected=\"false\"").count(), items);
    assert_eq!(html.matches("data-value=").count(), items);
    assert!(html.contains("data-value=\"gpt-4o\""));
    assert!(html.contains("128K context"), "{html}");
    assert!(html.contains("class=\"tag"), "source tag: {html}");
    assert!(!html.contains("<b>"), "upstream text is escaped: {html}");
    assert!(
        html.contains("&lt;b&gt;") || html.contains("&#60;b&#62;"),
        "{html}"
    );
    // htmx requests get the fragment too.
    let r =
        a.c.get(format!("{}/admin/search/models?q=gpt", a.url))
            .header("cookie", &a.cookie)
            .header("hx-request", "true")
            .send()
            .await
            .unwrap();
    assert!(r.text().await.unwrap().contains("role=\"option\""));
    // Admin only.
    let r =
        a.c.get(format!("{}/admin/search/models?q=gpt", a.url))
            .send()
            .await
            .unwrap();
    assert!(r.status().is_redirection());
}

#[tokio::test]
async fn multi_value_fields_accept_chips_and_comma_text() {
    let a = app().await;
    let (base, _) = upstream(vec![], Duration::ZERO).await;
    a.provider("openai", &base, "api_key").await;
    a.model("gpt-4o", "openai", "").await;
    a.model("gpt-4o-mini", "openai", "").await;
    db::create_user_with_limits(&a.st.db, "maria", None, None, false, Limits::default())
        .await
        .unwrap();
    // Chips: one field per value; the text field may hold more, comma-separated.
    let r = a
        .post(
            "/admin/keys",
            &[
                ("user_name", "maria"),
                ("allowed_models", "gpt-4o"),
                ("allowed_models", "openai/gpt-x"),
                ("allowed_models", "gpt-4o-mini, "),
            ],
        )
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let stored: String = sqlx::query_scalar("SELECT allowed_models FROM virtual_keys")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    let stored: Vec<String> = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored, ["gpt-4o", "openai/gpt-x", "gpt-4o-mini"]);

    // Aliases on the Model page: chips and comma text.
    let id =
        a.st.catalog()
            .models
            .iter()
            .find(|m| m.name == "gpt-4o")
            .unwrap()
            .id;
    let r = a
        .post(
            &format!("/admin/models/{id}"),
            &[
                ("provider", "openai"),
                ("aliases", "4o"),
                ("aliases", "omni, gpt-4o-latest"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let m =
        a.st.catalog()
            .models
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .unwrap();
    assert_eq!(m.aliases, ["4o", "omni", "gpt-4o-latest"]);

    // The add-model form too.
    let r = a
        .post(
            "/admin/models",
            &[
                ("name", "o3"),
                ("provider", "openai"),
                ("aliases", "o3-a"),
                ("aliases", "o3-b"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let m =
        a.st.catalog()
            .models
            .iter()
            .find(|m| m.name == "o3")
            .cloned()
            .unwrap();
    assert_eq!(m.aliases, ["o3-a", "o3-b"]);
}

#[tokio::test]
async fn fallback_order_is_kept_and_unknown_names_are_rejected() {
    let a = app().await;
    let (base, _) = upstream(vec![], Duration::ZERO).await;
    a.provider("openai", &base, "api_key").await;
    for m in ["a", "b", "c"] {
        a.model(m, "openai", "").await;
    }
    let row = [
        ("deployment_provider", "openai"),
        ("deployment_model", "gpt-4o"),
        ("deployment_weight", "1"),
    ];
    let mut form = vec![
        ("name", "chips"),
        ("strategy", "priority"),
        ("fallbacks", "c"),
        ("fallbacks", "a"),
        ("fallbacks", "b"),
    ];
    form.extend(row);
    assert_eq!(a.route(&form).await.status(), 303);
    let mut form = vec![
        ("name", "text"),
        ("strategy", "priority"),
        ("fallbacks", "b, c"),
    ];
    form.extend(row);
    assert_eq!(a.route(&form).await.status(), 303);
    let table = a.st.router_table();
    let fb = |n: &str| {
        table
            .routes
            .iter()
            .find(|r| r.name == n)
            .unwrap()
            .fallbacks
            .clone()
    };
    assert_eq!(fb("chips"), ["c", "a", "b"]);
    assert_eq!(fb("text"), ["b", "c"]);
    let page = a.page("/admin/routes").await;
    assert!(
        page.find("value=\"c, a, b\"").is_some(),
        "order kept in the form"
    );

    let mut form = vec![
        ("name", "bad"),
        ("strategy", "priority"),
        ("fallbacks", "a"),
        ("fallbacks", "nope"),
    ];
    form.extend(row);
    let r = a.route(&form).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Unknown Fallback: nope."));
}

#[tokio::test]
async fn model_inputs_render_as_pickers_with_a_plain_text_fallback() {
    let a = app().await;
    let (base, _) = upstream(vec![], Duration::ZERO).await;
    a.provider("openai", &base, "api_key").await;
    a.model("gpt-4o", "openai", "4o").await;
    db::create_user_with_limits(&a.st.db, "maria", None, None, false, Limits::default())
        .await
        .unwrap();
    let routes = a.page("/admin/routes").await;
    assert!(routes.contains("data-picker=\"single\" data-src=\"provider,model,models.dev\" data-scope=\"deployment_provider\""), "{routes}");
    assert!(routes.contains("data-picker=\"multi\" data-ordered data-src=\"route,model\""));
    // The row template for new Deployments has the picker too.
    let template = routes
        .split("<template id=\"deployment-row\">")
        .nth(1)
        .unwrap();
    assert!(template.contains("data-picker=\"single\""));
    let keys = a.page("/admin/keys").await;
    assert!(keys.contains("data-picker=\"multi\" data-src=\"model,route,provider\""));
    assert!(keys.contains("name=\"allowed_models\""));
    let ledger = a.page("/admin/ledger").await;
    assert!(ledger.contains("data-picker=\"single\" data-src=\"model,route\""));
    let usage = a.page("/admin/usage?model=gpt-4o").await;
    assert!(usage.contains("name=\"model\" value=\"gpt-4o\""), "{usage}");
    assert!(usage.contains("data-picker=\"single\""));
    let models = a.page("/admin/models").await;
    assert!(models.contains("data-scope=\"provider\""));
    let id = a.st.catalog().models[0].id;
    let model = a.page(&format!("/admin/models/{id}")).await;
    assert!(model.contains("data-picker=\"multi\""));
    assert!(model.contains("name=\"aliases\" value=\"4o\""));
    // No old datalist search on model fields.
    for page in [&routes, &keys, &ledger, &models, &model] {
        assert!(!page.contains("data-search=\"models\""));
    }
}

#[tokio::test]
async fn usage_model_filter_limits_the_rows() {
    let a = app().await;
    let user = db::create_user_with_limits(&a.st.db, "maria", None, None, false, Limits::default())
        .await
        .unwrap();
    let (key, _) = db::create_key_with_limits(&a.st.db, user, "", None, Limits::default())
        .await
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for model in ["gpt-4o", "o3-pro"] {
        db::insert_entry(
            &a.st.db,
            &db::NewEntry {
                route_rule: None,
                cache_hit: false,
                session_affinity: None,
                reported_cost_micros: None,
                ts_ms: now,
                user_id: user,
                key_id: key,
                team_id: None,
                provider: "openai".into(),
                subscription: false,
                endpoint: "chat_completions".into(),
                requested_model: Some(model.into()),
                model: Some(model.into()),
                status: 200,
                streamed: false,
                usage: Default::default(),
                quote: Default::default(),
                estimated: false,
                latency_ms: 10,
                ttft_ms: None,
                client_aborted: false,
                provider_key: Some(0),
                attempts: 1,
            },
        )
        .await
        .unwrap();
    }
    let all = a.page("/admin/usage").await;
    assert!(all.contains(">gpt-4o</th>") && all.contains(">o3-pro</th>"));
    let one = a.page("/admin/usage?model=o3-pro").await;
    assert!(one.contains(">o3-pro</th>"));
    assert!(!one.contains(">gpt-4o</th>"));
    let none = a.page("/admin/usage?model=none-such").await;
    assert!(none.contains("No requests in this range."));
}

/// models.dev models appear in the picker, scoped to the matching Provider,
/// from the bundled snapshot (no network).
#[tokio::test]
async fn models_dev_hits_are_scoped_to_the_mapped_provider() {
    let a = app().await;
    // An unreachable base URL: the live source fails, models.dev still answers.
    a.provider("deepseek", "https://api.deepseek.com", "api_key")
        .await;
    let hits = a
        .json("/admin/search/models?q=deepseek-v4&provider=deepseek")
        .await;
    let dev: Vec<&Value> = hits
        .iter()
        .filter(|h| h["source"] == "models.dev")
        .collect();
    assert!(
        dev.iter().any(|h| h["id"] == "deepseek-v4-pro"),
        "no models.dev hit: {hits:?}"
    );
    assert!(dev.iter().all(|h| h["provider"] == "deepseek"));
    assert!(
        dev.iter().any(|h| h["price"].is_string()),
        "no price: {dev:?}"
    );
    // A Provider with no models.dev match gets no models.dev hits.
    a.provider("mystery", "http://127.0.0.1:9/v1", "api_key")
        .await;
    let hits = a.json("/admin/search/models?q=gpt&provider=mystery").await;
    assert!(hits.iter().all(|h| h["source"] != "models.dev"), "{hits:?}");
}
