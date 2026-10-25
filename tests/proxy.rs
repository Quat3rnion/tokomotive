//! End-to-end tests: a mock Provider, the real Proxy, the real database.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt;
use llm_accounting::config::Config;
use llm_accounting::state::AppState;
use serde_json::{Value, json};
use sqlx::Row;

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(HeaderMap, Value)>>>);

fn sse(events: Vec<String>, delay_ms: u64) -> Response {
    let stream = futures_util::stream::iter(events).then(move |e| async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        Ok::<_, std::io::Error>(e)
    });
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn mock_openai(
    axum::extract::State(seen): axum::extract::State<Seen>,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    seen.0.lock().unwrap().push((headers, b.clone()));
    let usage = json!({"prompt_tokens": 1000, "completion_tokens": 100, "prompt_tokens_details": {"cached_tokens": 400}});
    if b["stream"] == json!(true) {
        let mut ev = vec![format!(
            "data: {}\n\n",
            json!({"model": "gpt-4o-2024-08-06", "choices": [{"delta": {"content": "hi"}}]})
        )];
        if b["stream_options"]["include_usage"] == json!(true) {
            ev.push(format!(
                "data: {}\n\n",
                json!({"model": "gpt-4o-2024-08-06", "choices": [], "usage": usage})
            ));
        }
        ev.push("data: [DONE]\n\n".into());
        return sse(ev, 5);
    }
    Json(json!({"model": b["model"], "choices": [], "usage": usage})).into_response()
}

async fn mock_anthropic(
    axum::extract::State(seen): axum::extract::State<Seen>,
    headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    seen.0.lock().unwrap().push((headers, b.clone()));
    let start_usage = json!({"input_tokens": 50, "output_tokens": 1, "cache_read_input_tokens": 2000,
        "cache_creation_input_tokens": 300, "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200}});
    if b["stream"] == json!(true) {
        let slow = b["metadata"]["slow"] == json!(true);
        let mut ev = vec![format!(
            "event: message_start\ndata: {}\n\n",
            json!({"type": "message_start", "message": {"model": "claude-sonnet-4-5-20250929", "usage": start_usage}})
        )];
        for _ in 0..(if slow { 20 } else { 2 }) {
            ev.push(format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type": "content_block_delta", "delta": {"text": "x"}})
            ));
        }
        ev.push(format!(
            "event: message_delta\ndata: {}\n\n",
            json!({"type": "message_delta", "usage": {"output_tokens": 77}})
        ));
        return sse(ev, if slow { 40 } else { 2 });
    }
    let mut u = start_usage;
    u["output_tokens"] = json!(77);
    Json(json!({"model": "claude-sonnet-4-5-20250929", "usage": u})).into_response()
}

struct Env {
    proxy: String,
    st: AppState,
    seen: Seen,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn env() -> Env {
    let seen = Seen::default();
    let upstream = axum::Router::new()
        .route("/v1/chat/completions", post(mock_openai))
        .route("/v1/messages", post(mock_anthropic))
        .with_state(seen.clone());
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = ul.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let cfg = format!(
        r#"
[server]
database = "sqlite://{db}"
admin_password = "pw"
admin_password_env = "LLMA_TEST_UNSET_VAR"
default_markup_percent = 20

[[providers]]
name = "openai"
kind = "openai"
base_url = "http://{u}"
api_key = "upstream-openai-key"

[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "http://{u}"
api_key = "upstream-anthropic-key"

[[models]]
name = "gpt-4o"
provider = "openai"
cost = {{ input = 2.5, output = 10, cache_read = 1.25 }}

[[models]]
name = "claude-sonnet-4-5"
provider = "anthropic"
cost = {{ input = 3, output = 15, cache_read = 0.3, cache_write_5m = 3.75, cache_write_1h = 6 }}
price = {{ markup_percent = 100 }}

[[teams]]
name = "acme"
price_overrides = [{{ model = "gpt-4o", markup_percent = 50 }}]

[[users]]
name = "alice"
team = "acme"
keys = ["sk-alice"]

[[users]]
name = "bob"
budget_usd = 0.01
keys = ["sk-bob"]

[[users]]
name = "carol"
keys = ["sk-carol"]
"#,
        db = dir.path().join("t.db").display(),
        u = uaddr
    );
    let cfg: Config = toml::from_str(&cfg).unwrap();
    cfg.validate().unwrap();
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    let app = llm_accounting::router(st.clone());
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let paddr = pl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });
    Env {
        proxy: format!("http://{paddr}"),
        st,
        seen,
        http: reqwest::Client::new(),
        _dir: dir,
    }
}

async fn last_entry(st: &AppState) -> sqlx::sqlite::SqliteRow {
    st.flush().await;
    sqlx::query("SELECT * FROM ledger ORDER BY id DESC LIMIT 1")
        .fetch_one(&st.db)
        .await
        .unwrap()
}

fn i(r: &sqlx::sqlite::SqliteRow, c: &str) -> i64 {
    r.get::<i64, _>(c)
}

#[tokio::test]
async fn openai_json_request_is_accounted_with_team_override() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-alice")
        .json(&json!({"model": "gpt-4o", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = last_entry(&e.st).await;
    // 600 uncached input, 400 cache read, 100 output
    assert_eq!(
        (i(&row, "input"), i(&row, "cache_read"), i(&row, "output")),
        (600, 400, 100)
    );
    // cost = 600*2.5 + 400*1.25 + 100*10 = 1500 + 500 + 1000 = 3000 micros
    assert_eq!(i(&row, "cost_micros"), 3000);
    // acme override: +50%
    assert_eq!(i(&row, "price_micros"), 4500);
    assert_eq!(i(&row, "unpriced"), 0);
    // The Provider Key replaced the Virtual Key.
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(seen[0].0["authorization"], "Bearer upstream-openai-key");
}

#[tokio::test]
async fn openai_stream_gets_include_usage_and_default_markup() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "gpt-4o", "stream": true, "messages": []}))
        .send()
        .await
        .unwrap();
    let text = r.text().await.unwrap();
    assert!(text.contains("[DONE]"));
    let row = last_entry(&e.st).await;
    assert_eq!(i(&row, "streamed"), 1);
    assert_eq!(i(&row, "estimated"), 0);
    assert_eq!(row.get::<String, _>("model"), "gpt-4o-2024-08-06");
    assert_eq!(i(&row, "cost_micros"), 3000);
    assert_eq!(i(&row, "price_micros"), 3600); // default 20%
    assert!(row.get::<Option<i64>, _>("ttft_ms").is_some());
}

#[tokio::test]
async fn anthropic_stream_cache_tiers() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/messages", e.proxy))
        .header("x-api-key", "sk-carol")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "claude-sonnet-4-5", "stream": true, "max_tokens": 10, "messages": []}))
        .send()
        .await
        .unwrap();
    r.text().await.unwrap();
    let row = last_entry(&e.st).await;
    assert_eq!(
        (
            i(&row, "input"),
            i(&row, "output"),
            i(&row, "cache_read"),
            i(&row, "cache_write_5m"),
            i(&row, "cache_write_1h")
        ),
        (50, 77, 2000, 100, 200)
    );
    // 50*3 + 77*15 + 2000*0.3 + 100*3.75 + 200*6 = 150 + 1155 + 600 + 375 + 1200
    assert_eq!(i(&row, "cost_micros"), 3480);
    assert_eq!(i(&row, "price_micros"), 6960);
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(seen[0].0["x-api-key"], "upstream-anthropic-key");
}

#[tokio::test]
async fn client_abort_still_records_full_usage() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/messages", e.proxy))
        .header("x-api-key", "sk-carol")
        .json(&json!({"model": "claude-sonnet-4-5", "stream": true, "metadata": {"slow": true}, "messages": []}))
        .send()
        .await
        .unwrap();
    let mut s = r.bytes_stream();
    s.next().await.unwrap().unwrap();
    drop(s); // client goes away mid-stream
    let row = last_entry(&e.st).await;
    assert_eq!(i(&row, "client_aborted"), 1);
    assert_eq!(i(&row, "estimated"), 0);
    assert_eq!(i(&row, "output"), 77);
}

#[tokio::test]
async fn auth_allowlist_and_budget() {
    let e = env().await;
    let post = |key: &'static str, model: &'static str| {
        e.http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth(key)
            .json(&json!({"model": model}))
            .send()
    };
    assert_eq!(post("nope", "gpt-4o").await.unwrap().status(), 401);

    // Budget $0.01 = 10_000 micros; each request is 3600 micros at +20%.
    for _ in 0..3 {
        assert_eq!(post("sk-bob", "gpt-4o").await.unwrap().status(), 200);
        e.st.flush().await;
    }
    assert_eq!(post("sk-bob", "gpt-4o").await.unwrap().status(), 402);

    // Restricted key.
    let alice: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'alice'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let (_, key) = llm_accounting::db::create_key(
        &e.st.db,
        alice,
        "t",
        Some(vec!["claude-sonnet-4-5".into()]),
    )
    .await
    .unwrap();
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth(&key)
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Revoked key.
    let kid: i64 = sqlx::query_scalar("SELECT id FROM virtual_keys ORDER BY id DESC LIMIT 1")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    llm_accounting::db::revoke_key(&e.st.db, kid).await.unwrap();
    let r = e
        .http
        .post(format!("{}/v1/messages", e.proxy))
        .bearer_auth(&key)
        .json(&json!({"model": "claude-sonnet-4-5"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn unpriced_model_is_flagged_then_repriced() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "gpt-9-preview"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = last_entry(&e.st).await;
    assert_eq!((i(&row, "unpriced"), i(&row, "cost_micros")), (1, 0));
    let before = llm_accounting::db::totals_since(&e.st.db, 0).await.unwrap();
    assert_eq!(before.cost_micros, 0);

    // Operator adds the card; reprice.
    let id = llm_accounting::db::create_model(&e.st.db, "gpt-9", "openai", &[])
        .await
        .unwrap();
    let card = llm_accounting::pricing::CardRates {
        base: llm_accounting::pricing::Rates {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        },
        tiers: vec![],
    };
    llm_accounting::db::add_cost_card(&e.st.db, id, &card)
        .await
        .unwrap();
    e.st.reload_catalog().await.unwrap();
    let n = llm_accounting::db::reprice_unpriced(&e.st.db, &e.st.catalog())
        .await
        .unwrap();
    assert_eq!(n, 1);
    let row = last_entry(&e.st).await;
    assert_eq!((i(&row, "unpriced"), i(&row, "cost_micros")), (0, 1100));
    // Rollups follow the ledger.
    let after = llm_accounting::db::totals_since(&e.st.db, 0).await.unwrap();
    assert_eq!((after.requests, after.cost_micros), (1, 1100));
}

#[tokio::test]
async fn dashboard_requires_login_and_renders_every_page() {
    let e = env().await;
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c.get(format!("{}/admin", e.proxy)).send().await.unwrap();
    assert_eq!(r.status(), 303);
    let bad = c
        .post(format!("{}/admin/login", e.proxy))
        .form(&[("password", "x")])
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    let r = c
        .post(format!("{}/admin/login", e.proxy))
        .form(&[("password", "pw")])
        .send()
        .await
        .unwrap();
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    // Some traffic so the tables have rows.
    e.http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-alice")
        .json(&json!({"model": "gpt-4o"}))
        .send()
        .await
        .unwrap();
    e.st.flush().await;

    for path in [
        "/admin",
        "/admin?range=7d",
        "/admin?range=30d",
        "/admin/users",
        "/admin/teams",
        "/admin/keys",
        "/admin/models",
        "/admin/models/1",
        "/admin/ledger",
        "/admin/ledger?filter=unpriced",
    ] {
        let r = c
            .get(format!("{}{}", e.proxy, path))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let body = r.text().await.unwrap();
        assert!(body.contains("LLM Accounting"), "{path}");
    }
    let body = c
        .get(format!("{}/admin", e.proxy))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("$0.0045"), "price of alice request shown");

    // Create a key through the form; the plain key is shown once.
    let r = c
        .post(format!("{}/admin/keys", e.proxy))
        .header("cookie", &cookie)
        .form(&[("user_id", "1"), ("label", "x"), ("allowed_models", "")])
        .send()
        .await
        .unwrap();
    assert!(r.text().await.unwrap().contains("sk-llma-"));

    // New Price Card version through the form.
    let r = c
        .post(format!("{}/admin/models/1/price", e.proxy))
        .header("cookie", &cookie)
        .form(&[("mode", "markup"), ("markup_percent", "30")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let m = e.st.catalog();
    assert!(matches!(
        m.resolve("gpt-4o").unwrap().price,
        Some((_, llm_accounting::pricing::PriceSpec::MarkupBp(3000)))
    ));
}
