//! Models and Model detail pages: Cost Cards, Price Cards and their forms.

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};
use sqlx::Row;
use tokomotive::{
    config::Config,
    db,
    pricing::{CardRates, PriceSpec, Rates},
    ratelimit::Limits,
    state::AppState,
    usage::Usage,
};

#[tokio::test]
async fn models_dev_details_mapping_pin_and_settings_are_visible() {
    let a = app().await;
    a.provider().await;
    let id = db::create_model(&a.st.db, "claude-sonnet-4-5", "openai", &[])
        .await
        .unwrap();
    a.st.reload_catalog().await.unwrap();
    let page = a.page(&format!("/admin/models/{id}")).await;
    assert!(page.contains("Not in models.dev"));
    let r = a
        .post(
            &format!("/admin/models/{id}/models-dev"),
            &[
                ("models_dev_provider", "anthropic"),
                ("models_dev_model", "claude-sonnet-4-5"),
                ("cost_pinned", "on"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    let page = a.page(&format!("/admin/models/{id}")).await;
    for text in [
        "models.dev details",
        "200,000",
        "64,000",
        "Reasoning",
        "Tool calls",
        "Structured output",
        "2025-07-31",
        "deprecated",
        "Pinned: models.dev sync does not change this card",
    ] {
        assert!(page.contains(text), "{text}");
    }
    let settings = a.page("/admin/settings").await;
    assert!(settings.contains("Refresh model data"));
    assert!(settings.contains("https://models.dev/api.json"));
    let cost = CardRates {
        base: Rates {
            input: 1_000_000,
            ..Default::default()
        },
        tiers: vec![],
    };
    sqlx::query(
        "INSERT INTO models_dev_alerts(model_id,old_spec,new_spec,created_at) VALUES(?,?,?,0)",
    )
    .bind(id)
    .bind(serde_json::to_string(&cost).unwrap())
    .bind(serde_json::to_string(&cost.marked_up(10_000)).unwrap())
    .execute(&a.st.db)
    .await
    .unwrap();
    let overview = a.page("/admin").await;
    assert!(overview.contains("Price changed a lot. Check it."));
    assert!(overview.contains(&format!("/admin/models/{id}#cost")));
}

#[tokio::test]
async fn models_dev_refresh_uses_saved_url_etag_and_keeps_old_data_on_error() {
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    let a = app().await;
    let mock = axum::Router::new()
        .route(
            "/data",
            get(|h: HeaderMap| async move {
                assert_eq!(h["if-none-match"], "\"test\"");
                StatusCode::NOT_MODIFIED
            }),
        )
        .route("/bad", get(|| async { "not json" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    assert_eq!(
        a.post(
            "/admin/settings/models-dev",
            &[("url", &format!("{url}/data"))]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        tokomotive::settings::value(&a.st.db, "models_dev_enabled")
            .await
            .unwrap()
            .as_deref(),
        Some("false")
    );
    tokomotive::settings::put(&a.st.db, "models_dev_etag", "\"test\"")
        .await
        .unwrap();
    assert_eq!(
        a.post("/admin/settings/models-dev/refresh", &[])
            .await
            .status(),
        303
    );
    assert_eq!(
        tokomotive::settings::value(&a.st.db, "models_dev_etag")
            .await
            .unwrap()
            .as_deref(),
        Some("\"test\"")
    );
    assert_eq!(
        a.post(
            "/admin/settings/models-dev",
            &[("url", &format!("{url}/bad"))]
        )
        .await
        .status(),
        303
    );
    assert_eq!(
        tokomotive::settings::value(&a.st.db, "models_dev_etag")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        a.post("/admin/settings/models-dev/refresh", &[])
            .await
            .status(),
        400
    );
    let page = a.page("/admin/settings").await;
    assert!(page.contains("Invalid models.dev payload"));
    assert_eq!(
        tokomotive::models_dev::search(&a.st.db, Some("anthropic"), "sonnet", 1)
            .await
            .unwrap()
            .len(),
        1
    );
}

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
    cookie: String,
}

async fn upstream(Json(b): Json<Value>) -> Json<Value> {
    Json(json!({
        "id": "chat_test",
        "model": b["model"],
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 100}
    }))
}

async fn app() -> App {
    app_in("full").await
}

async fn app_in(mode: &str) -> App {
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
        .form(&[("password", "long-password"), ("mode", mode)])
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
        let r = self
            .c
            .get(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        r.text().await.unwrap()
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

    async fn post_htmx(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("hx-request", "true")
            .form(form)
            .send()
            .await
            .unwrap()
    }

    /// A Provider that points to a mock OpenAI upstream.
    async fn provider(&self) {
        let up = axum::Router::new().route("/v1/chat/completions", post(upstream));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, up).await.unwrap() });
        let r = self
            .post(
                "/admin/providers",
                &[
                    ("name", "openai"),
                    ("kind", "openai"),
                    ("base_url", base.as_str()),
                    ("api_key", "sk-test-1234"),
                ],
            )
            .await;
        assert_eq!(r.status(), 303);
    }

    async fn add_model(&self, form: &[(&str, &str)]) -> reqwest::Response {
        self.post("/admin/models", form).await
    }

    fn model_id(&self, name: &str) -> i64 {
        self.st.catalog().resolve(name).unwrap().id
    }
}

/// The text of the first form whose action is exactly `action`.
fn form<'a>(html: &'a str, action: &str) -> &'a str {
    let at = html
        .find(&format!("action=\"{action}\""))
        .unwrap_or_else(|| panic!("no form {action}"));
    let end = html[at..].find("</form>").unwrap() + at;
    &html[at..end]
}

/// All forms with this action.
fn forms<'a>(html: &'a str, action: &str) -> Vec<&'a str> {
    let needle = format!("action=\"{action}\"");
    html.match_indices(&needle)
        .map(|(at, _)| {
            let end = html[at..].find("</form>").unwrap() + at;
            &html[at..end]
        })
        .collect()
}

// ----------------------------------------------------------------- M-01

#[tokio::test]
async fn blank_rates_add_an_unpriced_model_and_its_requests_are_flagged() {
    let a = app().await;
    a.provider().await;
    let html = a.page("/admin/models").await;
    let add = form(&html, "/admin/models");
    for f in ["input", "output", "cache_read", "image_output"] {
        assert!(
            !add.contains(&format!("name=\"{f}\" value=\"0\"")),
            "{f} is pre-filled: {add}"
        );
    }
    assert!(add.contains("Leave all rates blank"), "{add}");

    let blank = [
        ("name", "gpt-test"),
        ("provider", "openai"),
        ("aliases", ""),
        ("input", ""),
        ("output", ""),
        ("cache_read", ""),
        ("cache_write_5m", ""),
        ("cache_write_1h", ""),
        ("image_input", ""),
        ("image_output", ""),
        ("tiers", ""),
    ];
    let r = a.add_model(&blank).await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let cat = a.st.catalog();
    let m = cat.resolve("gpt-test").unwrap();
    assert!(m.cost.is_none(), "no $0 Cost Card");
    assert!(
        a.st.catalog()
            .quote(Some("gpt-test"), None, &Usage::default())
            .unpriced
    );
    assert!(db::card_history(&a.st.db, m.id).await.unwrap().is_empty());

    let html = a.page("/admin/models").await;
    assert!(html.contains("Unpriced"), "{html}");
    let html = a.page(&format!("/admin/models/{}", m.id)).await;
    assert!(html.contains("no Cost Card"), "{html}");
    // The Cost Card form starts empty, not with 0.
    assert!(!form(&html, &format!("/admin/models/{}/cost", m.id)).contains("value=\"0.00\""));

    // A real request through the Proxy is flagged as Unpriced.
    let user = db::create_user_with_limits(&a.st.db, "maria", None, None, false, Limits::default())
        .await
        .unwrap();
    let (_, key) = db::create_key_with_limits(&a.st.db, user, "t", None, Limits::default())
        .await
        .unwrap();
    let r =
        a.c.post(format!("{}/v1/chat/completions", a.url))
            .bearer_auth(&key)
            .json(&json!({"model": "gpt-test", "messages": []}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    a.st.flush().await;
    let row = sqlx::query("SELECT unpriced, price_micros FROM ledger ORDER BY id DESC LIMIT 1")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("unpriced"), 1);
    assert_eq!(row.get::<i64, _>("price_micros"), 0);
}

#[tokio::test]
async fn a_zero_cost_card_needs_a_confirmation() {
    let a = app().await;
    a.provider().await;
    let zero = [
        ("name", "local"),
        ("provider", "openai"),
        ("input", "0"),
        ("output", "0"),
    ];
    let r = a.add_model(&zero).await;
    assert_eq!(r.status(), 400);
    let body = r.text().await.unwrap();
    assert!(body.contains("on purpose"), "{body}");
    assert!(a.st.catalog().resolve("local").is_none(), "nothing saved");

    let mut ok = zero.to_vec();
    ok.push(("allow_zero", "1"));
    assert_eq!(a.add_model(&ok).await.status(), 303);
    let cat = a.st.catalog();
    let m = cat.resolve("local").unwrap();
    assert_eq!(m.cost.as_ref().unwrap().1, CardRates::default());

    // The same rule on the Model page.
    let id = m.id;
    let r = a
        .post(
            &format!("/admin/models/{id}/cost"),
            &[("input", "0"), ("output", "")],
        )
        .await;
    assert_eq!(r.status(), 400);
    let r = a
        .post(&format!("/admin/models/{id}/cost"), &[("input", "")])
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Enter the rates"));
}

#[tokio::test]
async fn rates_must_be_numbers_of_zero_or_more_and_tiers_need_six_or_eight_numbers() {
    let a = app().await;
    a.provider().await;
    let r = a
        .add_model(&[("name", "x"), ("provider", "openai"), ("input", "-1")])
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("0 or more"));
    let r = a
        .add_model(&[
            ("name", "x"),
            ("provider", "openai"),
            ("input", "1"),
            ("output", "2"),
            ("tiers", "200000 2 4"),
        ])
        .await;
    assert_eq!(r.status(), 400);
    let body = r.text().await.unwrap();
    assert!(body.contains("Context Tier line 1 has 3 numbers"), "{body}");
    assert!(a.st.catalog().resolve("x").is_none());
}

// ----------------------------------------------------------------- MD-01

async fn priced_model(a: &App) -> i64 {
    a.provider().await;
    let r = a
        .add_model(&[
            ("name", "haiku"),
            ("provider", "openai"),
            ("input", "0.10"),
            ("output", "0.50"),
            ("cache_read", "0.01"),
            ("tiers", "100000 0.50 2.50 0.05 0.625 1.00"),
        ])
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    a.model_id("haiku")
}

#[tokio::test]
async fn price_card_and_team_override_forms_are_not_prefilled_with_cost() {
    let a = app().await;
    let id = priced_model(&a).await;
    db::create_team(&a.st.db, "research").await.unwrap();
    let html = a.page(&format!("/admin/models/{id}")).await;

    let cost = form(&html, &format!("/admin/models/{id}/cost"));
    assert!(
        cost.contains("value=\"0.10\""),
        "the Cost Card form shows the Cost Card"
    );

    let price = form(&html, &format!("/admin/models/{id}/price"));
    assert!(!price.contains("value=\"0.10\""), "{price}");
    assert!(!price.contains("value=\"0.50\""), "{price}");
    assert!(!price.contains(">100000"), "{price}");
    assert!(price.contains("Copy Cost Card rates"), "{price}");

    let team = forms(&html, &format!("/admin/models/{id}/team-price"));
    let add = team.last().unwrap();
    assert!(add.contains("name=\"team_name\""));
    assert!(!add.contains("value=\"0.10\""), "{add}");
    assert!(!add.contains(">100000"), "{add}");
}

#[tokio::test]
async fn explicit_price_rates_are_saved_as_entered() {
    let a = app().await;
    let id = priced_model(&a).await;

    // Blank explicit rates do not copy the Cost Card.
    let r = a
        .post(&format!("/admin/models/{id}/price"), &[("mode", "rates")])
        .await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Enter the Price rates"));
    assert!(a.st.catalog().resolve("haiku").unwrap().price.is_none());

    let r = a
        .post(
            &format!("/admin/models/{id}/price"),
            &[("mode", "rates"), ("input", "0.20"), ("output", "1.00")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let cat = a.st.catalog();
    let m = cat.resolve("haiku").unwrap();
    assert_eq!(
        m.price.as_ref().unwrap().1,
        PriceSpec::Rates(CardRates {
            base: Rates {
                input: 200_000,
                output: 1_000_000,
                ..Default::default()
            },
            tiers: vec![],
        })
    );
    // Below the Context Tier: Cost 0.10 and 0.50, Price 0.20 and 1.00.
    let usage = Usage {
        input: 100_000,
        output: 1_000_000,
        ..Default::default()
    };
    let q = cat.quote(Some("haiku"), None, &usage);
    assert_eq!((q.cost_micros, q.price_micros), (510_000, 1_020_000));

    // The page shows the explicit Price Card in its own form.
    let html = a.page(&format!("/admin/models/{id}")).await;
    let price = form(&html, &format!("/admin/models/{id}/price"));
    assert!(price.contains("value=\"0.20\""), "{price}");
}

#[tokio::test]
async fn a_blank_markup_gets_a_plain_message() {
    let a = app().await;
    let id = priced_model(&a).await;
    let team = db::create_team(&a.st.db, "research")
        .await
        .unwrap()
        .to_string();
    for (path, form) in [
        (
            format!("/admin/models/{id}/price"),
            vec![("mode", "markup"), ("markup_percent", "")],
        ),
        (
            format!("/admin/models/{id}/team-price"),
            vec![
                ("team_id", team.as_str()),
                ("mode", "markup"),
                ("markup_percent", " "),
            ],
        ),
    ] {
        let r = a.post_htmx(&path, &form).await;
        assert_eq!(r.status(), 400, "{path}");
        let body = r.text().await.unwrap();
        assert!(body.contains("Enter the Markup"), "{body}");
    }
}

// ----------------------------------------------------------------- M-02, M-03

#[tokio::test]
async fn without_providers_the_page_says_to_add_one_first() {
    let a = app().await;
    let html = a.page("/admin/models").await;
    assert!(
        html.contains("Add a Provider before you add a Model."),
        "{html}"
    );
    assert!(html.contains("href=\"/admin/providers\""));
    assert!(
        !html.contains("name=\"provider\""),
        "no empty Provider select"
    );
}

#[tokio::test]
async fn a_duplicate_name_gets_a_plain_message_next_to_the_form() {
    let a = app().await;
    a.provider().await;
    let r = a
        .add_model(&[
            ("name", "gpt-4o"),
            ("provider", "openai"),
            ("aliases", "4o"),
        ])
        .await;
    assert_eq!(r.status(), 303);
    for (name, aliases, msg) in [
        ("gpt-4o", "", "A Model named gpt-4o already exists."),
        ("4o", "", "4o is already an alias of Model gpt-4o."),
        ("other", "gpt-4o", "gpt-4o is already used by Model gpt-4o."),
    ] {
        let r = a
            .post_htmx(
                "/admin/models",
                &[("name", name), ("provider", "openai"), ("aliases", aliases)],
            )
            .await;
        assert_eq!(r.status(), 400, "{name}");
        // The error goes to the form's slot, so the page and its input stay.
        assert_eq!(r.headers()["hx-retarget"], "find .form-error");
        let body = r.text().await.unwrap();
        assert!(body.contains(msg), "{body}");
        assert!(!body.contains("UNIQUE"), "{body}");
    }
    assert_eq!(a.st.catalog().models.len(), 1);
    let html = a.page("/admin/models").await;
    assert!(form(&html, "/admin/models").contains("class=\"form-error\""));
}

// ----------------------------------------------------------------- M-04..M-06

#[tokio::test]
async fn default_markup_has_one_editor_on_the_models_page() {
    let a = app().await;
    let html = a.page("/admin/models").await;
    assert!(html.contains("id=\"markup\""), "{html}");
    let r = a
        .post(
            "/admin/settings/markup",
            &[("default_markup_percent", "25")],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/models#markup");
    assert_eq!(a.st.catalog().default_markup_bp, 2500);
}

#[tokio::test]
async fn tiers_and_rates_have_labels_units_and_short_price_tags() {
    let a = app().await;
    let id = priced_model(&a).await;
    let html = a.page("/admin/models").await;
    assert!(html.contains("$ per million tokens"), "{html}");
    assert!(html.contains("+0% default"), "{html}");
    assert!(html.contains("<label for=\"new-tiers\">"), "{html}");
    assert!(
        html.contains("aria-describedby=\"new-tiers-help\""),
        "{html}"
    );

    let html = a.page(&format!("/admin/models/{id}")).await;
    for p in ["cost", "price"] {
        assert!(html.contains(&format!("<label for=\"{p}-tiers\">")), "{p}");
    }
    // MD-03: plain card names and a way back to the list.
    assert!(html.contains("aria-label=\"Breadcrumb\""), "{html}");
    assert!(html.contains("Cost Card</td>"), "{html}");
    assert!(html.contains("Added (UTC)"), "{html}");
}

#[tokio::test]
async fn simple_mode_adds_models_without_cards() {
    let a = app_in("simple").await;
    a.provider().await;
    let html = a.page("/admin/models").await;
    assert!(!html.contains("name=\"input\""));
    assert!(!html.contains("id=\"markup\""));
    let r = a
        .add_model(&[("name", "gpt-4o"), ("provider", "openai"), ("input", "3")])
        .await;
    assert_eq!(r.status(), 303);
    assert!(a.st.catalog().resolve("gpt-4o").unwrap().cost.is_none());
    let html = a
        .page(&format!("/admin/models/{}", a.model_id("gpt-4o")))
        .await;
    assert!(!html.contains("/cost\""));
}
