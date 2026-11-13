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
    seen.0.lock().unwrap().push((headers.clone(), b.clone()));
    if headers["authorization"] == "Bearer bad-key" {
        return (axum::http::StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
    }
    if headers["authorization"] == "Bearer fail-key" {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();
    }
    let usage = json!({"prompt_tokens": 1000, "completion_tokens": 100, "prompt_tokens_details": {"cached_tokens": 400}});
    if b["metadata"]["omit_model"] == json!(true) {
        return Json(json!({"choices": [], "usage": usage})).into_response();
    }
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

async fn mock_embeddings(Json(b): Json<Value>) -> Response {
    Json(json!({"model": b["model"], "data": [], "usage": {"prompt_tokens": 1000, "total_tokens": 1000}})).into_response()
}

async fn mock_images(Json(_): Json<Value>) -> Response {
    Json(
        json!({"data": [], "usage": {"input_tokens": 300, "output_tokens": 1000,
        "input_tokens_details": {"text_tokens": 100, "image_tokens": 200}}}),
    )
    .into_response()
}

struct Env {
    proxy: String,
    st: AppState,
    seen: Seen,
    http: reqwest::Client,
    cfg: Config,
    _dir: tempfile::TempDir,
}

async fn env() -> Env {
    env_with_routes("").await
}

async fn env_with_routes(routes: &str) -> Env {
    env_with_config(routes, true).await
}

async fn env_with_config(routes: &str, with_models: bool) -> Env {
    let seen = Seen::default();
    let upstream = axum::Router::new()
        .route("/v1/chat/completions", post(mock_openai))
        .route("/v1/messages", post(mock_anthropic))
        .route("/v1/embeddings", post(mock_embeddings))
        .route("/v1/images/generations", post(mock_images))
        .with_state(seen.clone());
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uaddr = ul.local_addr().unwrap();
    let routes = routes.replace("$UPSTREAM", &uaddr.to_string());
    tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let cfg = format!(
        r#"
[server]
database = "sqlite://{db}"
admin_password = "pw"
admin_password_env = "LLMA_TEST_UNSET_VAR"
metrics_token = "mtok"
metrics_token_env = "LLMA_TEST_UNSET_VAR"
default_markup_percent = 20

{routes}

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

[[providers]]
name = "pool"
kind = "openai"
base_url = "http://{u}"
api_keys = ["bad-key", "good-key"]

[[models]]
name = "gpt-pool"
provider = "pool"
cost = {{ input = 1, output = 1 }}

[[models]]
name = "text-embedding-3-small"
provider = "openai"
cost = {{ input = 0.02 }}

[[models]]
name = "gpt-image-2"
provider = "openai"
cost = {{ input = 5, cache_read = 1.25, image_input = 8, image_output = 30 }}

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
    let mut cfg: Config = toml::from_str(&cfg).unwrap();
    if !with_models {
        cfg.models.clear();
    }
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
        cfg,
        _dir: dir,
    }
}

#[tokio::test]
async fn explicit_route_rewrites_and_prices_the_deployment_model() {
    let e = env_with_routes(
        r#"
[[routes]]
name = "smart"
deployments = [{ provider = "openai", model = "gpt-4o" }]
"#,
    )
    .await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "smart", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(e.seen.0.lock().unwrap()[0].1["model"], "gpt-4o");
    let row = last_entry(&e.st).await;
    assert_eq!(row.get::<String, _>("requested_model"), "smart");
    assert_eq!(row.get::<String, _>("model"), "gpt-4o");
    assert_eq!(row.get::<String, _>("provider"), "openai");
    assert_eq!(i(&row, "cost_micros"), 3000);
    assert_eq!(i(&row, "price_micros"), 3600);
}

#[tokio::test]
async fn route_falls_back_after_all_keys_fail_and_skips_open_circuit() {
    let e = env_with_routes(
        r#"
[router]
failure_threshold = 2
cooldown_secs = 30
[[providers]]
name = "down"
kind = "openai"
base_url = "http://$UPSTREAM"
api_keys = ["bad-key", "fail-key"]
[[routes]]
name = "smart"
deployments = [{ provider = "down", model = "gpt-4o" }]
fallbacks = ["backup", "smart", "missing"]
[[routes]]
name = "backup"
deployments = [{ provider = "openai", model = "gpt-4o" }]
fallbacks = ["smart"]
"#,
    )
    .await;
    for attempts in [3, 1] {
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model": "smart"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let row = last_entry(&e.st).await;
        assert_eq!(i(&row, "attempts"), attempts);
        assert_eq!(row.get::<String, _>("provider"), "openai");
    }
    assert_eq!(e.seen.0.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn route_strategies_distribute_first_choices_and_rotation_survives_table_swap() {
    for strategy in ["round_robin", "weighted"] {
        let cfg = r#"
[[providers]]
name = "second"
kind = "openai"
base_url = "http://$UPSTREAM"
api_key = "second-key"
[[routes]]
name = "balanced"
strategy = "$STRATEGY"
deployments = [
  { provider = "openai", model = "gpt-4o", weight = 3 },
  { provider = "second", model = "gpt-4o", weight = 1 },
]
"#
        .replace("$STRATEGY", strategy);
        let e = env_with_routes(&cfg).await;
        for n in 0..120 {
            if n == 5 {
                e.st.set_router_table((*e.st.router_table()).clone());
            }
            let r = e
                .http
                .post(format!("{}/v1/chat/completions", e.proxy))
                .bearer_auth("sk-carol")
                .json(&json!({"model": "balanced"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
        }
        let seen = e.seen.0.lock().unwrap();
        let first = seen
            .iter()
            .filter(|(h, _)| h["authorization"] == "Bearer upstream-openai-key")
            .count();
        if strategy == "round_robin" {
            assert_eq!(first, 60);
            assert!(
                seen.windows(2)
                    .all(|pair| pair[0].0["authorization"] != pair[1].0["authorization"])
            );
        } else {
            assert!(
                (65..=110).contains(&first),
                "first deployment chosen {first}/120 times"
            );
        }
    }
}

#[tokio::test]
async fn incompatible_deployment_is_skipped() {
    let e = env_with_routes(
        r#"
[[routes]]
name = "smart"
deployments = [
 { provider = "anthropic", model = "claude-sonnet-4-5" },
 { provider = "openai", model = "gpt-4o" },
]
"#,
    )
    .await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "smart"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(e.seen.0.lock().unwrap().len(), 1);
    assert_eq!(e.seen.0.lock().unwrap()[0].1["model"], "gpt-4o");
    assert_eq!(i(&last_entry(&e.st).await, "attempts"), 1);
}

#[tokio::test]
async fn model_endpoints_authenticate_filter_and_return_native_formats_without_ledger_entries() {
    let e = env_with_routes(
        r#"
[[routes]]
name = "smart"
deployments = [{ provider = "openai", model = "gpt-4o" }]
[[routes]]
name = "gpt-4o"
deployments = [{ provider = "openai", model = "gpt-4o" }]
"#,
    )
    .await;
    let url = format!("{}/v1/models", e.proxy);
    assert_eq!(e.http.get(&url).send().await.unwrap().status(), 401);
    let all: Value = e
        .http
        .get(&url)
        .bearer_auth("sk-carol")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all["object"], "list");
    assert_eq!(all["data"].as_array().unwrap().len(), 6);
    assert!(
        all["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "smart" && m["owned_by"] == "router")
    );
    assert_eq!(
        all["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["id"] == "gpt-4o")
            .count(),
        1
    );
    let carol: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'carol'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let (_, key) =
        llm_accounting::db::create_key(&e.st.db, carol, "routes", Some(vec!["smart".into()]))
            .await
            .unwrap();
    let listed: Value = e
        .http
        .get(&url)
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed["data"],
        json!([{"id":"smart", "object":"model", "created":0, "owned_by":"router"}])
    );
    let anthropic: Value = e
        .http
        .get(&url)
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        anthropic,
        json!({"data":[{"type":"model", "id":"smart", "display_name":"smart", "created_at":"1970-01-01T00:00:00Z"}], "has_more":false, "first_id":"smart", "last_id":"smart"})
    );
    let single: Value = e
        .http
        .get(format!("{url}/smart"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(single["id"], "smart");
    for id in ["missing", "gpt-4o"] {
        let r = e
            .http
            .get(format!("{url}/{id}"))
            .bearer_auth(&key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(r.json::<Value>().await.unwrap()["type"], "error");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        0
    );
    assert!(e.seen.0.lock().unwrap().is_empty());
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth(&key)
        .json(&json!({"model":"smart"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn provider_prefix_rewrites_prices_and_accepts_canonical_or_exact_allowlist() {
    let e = env().await;
    let carol: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'carol'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    for allowed in ["gpt-4o", "openai/gpt-4o"] {
        let (_, key) =
            llm_accounting::db::create_key(&e.st.db, carol, "prefix", Some(vec![allowed.into()]))
                .await
                .unwrap();
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth(&key)
            .json(&json!({"model":"openai/gpt-4o"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let row = last_entry(&e.st).await;
        assert_eq!(row.get::<String, _>("requested_model"), "openai/gpt-4o");
        assert_eq!(row.get::<String, _>("model"), "gpt-4o");
        assert_eq!(i(&row, "cost_micros"), 3000);
    }
    assert!(
        e.seen
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(_, b)| b["model"] == "gpt-4o")
    );
}

#[tokio::test]
async fn no_catalog_or_routes_forwards_and_records_unpriced_usage() {
    let e = env_with_config("", false).await;
    assert!(e.cfg.models.is_empty() && e.cfg.routes.is_empty());
    for model in ["openai/new-model", "unknown/new-model"] {
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model":model}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let row = last_entry(&e.st).await;
        assert_eq!(i(&row, "unpriced"), 1);
        assert_eq!((i(&row, "input"), i(&row, "output")), (600, 100));
        assert_eq!(row.get::<String, _>("requested_model"), model);
    }
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(seen[0].1["model"], "new-model");
    assert_eq!(seen[1].1["model"], "unknown/new-model");
}

#[tokio::test]
async fn route_uses_deployment_price_without_response_model_and_reserves_most_expensive_model() {
    let e = env_with_routes(
        r#"
[[routes]]
name = "smart"
deployments = [{ provider = "openai", model = "gpt-4o" }]
fallbacks = ["expensive"]
[[routes]]
name = "expensive"
deployments = [{ provider = "openai", model = "claude-sonnet-4-5" }]
"#,
    )
    .await;
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model":"smart", "metadata":{"omit_model":true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = last_entry(&e.st).await;
    assert_eq!(row.get::<String, _>("model"), "gpt-4o");
    assert_eq!(i(&row, "cost_micros"), 3000);
    // 500 output tokens fit on gpt-4o ($0.006 at +20%) but not the
    // fallback ($0.015 at +100%). No upstream request may start.
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-bob")
        .json(&json!({"model":"smart", "max_tokens":500}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402);
    assert_eq!(e.seen.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn attempt_cap_and_all_open_degraded_mode_still_return_an_upstream_response() {
    let e = env_with_routes(
        r#"
[router]
max_attempts = 1
failure_threshold = 1
[[providers]]
name = "down"
kind = "openai"
base_url = "http://$UPSTREAM"
api_keys = ["bad-key", "fail-key"]
[[routes]]
name = "only-down"
deployments = [{ provider = "down", model = "gpt-4o" }]
"#,
    )
    .await;
    for status in [429, 503] {
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model":"only-down"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status);
        assert_eq!(i(&last_entry(&e.st).await, "attempts"), 1);
        // A runtime table swap must not discard the open circuit.
        e.st.set_router_table(llm_accounting::routing::RouterTable::from_config(&e.cfg));
    }
    assert_eq!(e.seen.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn degraded_mode_tries_the_entire_key_pool() {
    let e = env_with_routes(
        r#"
[router]
failure_threshold = 1
[[providers]]
name = "down"
kind = "openai"
base_url = "http://$UPSTREAM"
api_keys = ["bad-key", "fail-key"]
[[routes]]
name = "only-down"
deployments = [{ provider = "down", model = "gpt-4o" }]
"#,
    )
    .await;
    for _ in 0..2 {
        e.http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model":"only-down"}))
            .send()
            .await
            .unwrap();
        assert_eq!(i(&last_entry(&e.st).await, "attempts"), 2);
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
    // Rollups group the dated name under the matched Model.
    let groups = llm_accounting::db::by_model(&e.st.db, 0).await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key, "gpt-4o");
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
            .json(&json!({"model": model, "max_tokens": 100}))
            .send()
    };
    assert_eq!(post("nope", "gpt-4o").await.unwrap().status(), 401);

    // Budget $0.01 = 10_000 micros; each request is 3600 micros at +20%.
    // The reservation for each is about 1_200 micros (100 output tokens).
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
        .post(format!(
            "{}/admin/models/{}/price",
            e.proxy,
            e.st.catalog().resolve("gpt-4o").unwrap().id
        ))
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

#[tokio::test]
async fn key_pool_retries_with_next_key_on_429() {
    let e = env().await;
    for _ in 0..2 {
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model": "gpt-pool"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    e.st.flush().await;
    let rows = sqlx::query("SELECT provider_key, attempts, status FROM ledger ORDER BY id")
        .fetch_all(&e.st.db)
        .await
        .unwrap();
    // Round-robin: one request starts on the bad key and retries.
    let attempts: Vec<i64> = rows.iter().map(|r| i(r, "attempts")).collect();
    let mut sorted = attempts.clone();
    sorted.sort();
    assert_eq!(sorted, vec![1, 2]);
    assert!(
        rows.iter()
            .all(|r| i(r, "provider_key") == 1 && i(r, "status") == 200)
    );
}

#[tokio::test]
async fn injected_usage_chunk_is_not_sent_to_client() {
    let e = env().await;
    let text = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "gpt-4o", "stream": true}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        text.contains("[DONE]") && !text.contains("\"usage\""),
        "{text}"
    );
    assert_eq!(i(&last_entry(&e.st).await, "output"), 100);

    // A client that asked for usage still gets it.
    let text = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(
            &json!({"model": "gpt-4o", "stream": true, "stream_options": {"include_usage": true}}),
        )
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("\"usage\""));
}

#[tokio::test]
async fn embeddings_and_images_are_priced() {
    let e = env().await;
    let r = e
        .http
        .post(format!("{}/v1/embeddings", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "text-embedding-3-small", "input": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = last_entry(&e.st).await;
    assert_eq!(row.get::<String, _>("endpoint"), "embeddings");
    assert_eq!(
        (
            i(&row, "input"),
            i(&row, "cost_micros"),
            i(&row, "unpriced")
        ),
        (1000, 20, 0)
    );

    let r = e
        .http
        .post(format!("{}/v1/images/generations", e.proxy))
        .bearer_auth("sk-carol")
        .json(&json!({"model": "gpt-image-2", "prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row = last_entry(&e.st).await;
    assert_eq!(
        (
            i(&row, "input"),
            i(&row, "image_input"),
            i(&row, "image_output")
        ),
        (100, 200, 1000)
    );
    // 100*5 + 200*8 + 1000*30 = 500 + 1600 + 30000
    assert_eq!(i(&row, "cost_micros"), 32_100);
}

#[tokio::test]
async fn budget_reservation_covers_worst_case() {
    let e = env().await;
    // bob has $0.01. 1M output tokens at $12/M cannot fit.
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-bob")
        .json(&json!({"model": "gpt-4o", "max_tokens": 1_000_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402);

    // Open reservations count against the Budget.
    let bob: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'bob'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let held = e.st.reserve(bob, 10_000, 0, 9_000).unwrap();
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-bob")
        .json(&json!({"model": "gpt-4o", "max_tokens": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 402);
    drop(held);
    assert_eq!(e.st.reserved(bob), 0);
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-bob")
        .json(&json!({"model": "gpt-4o", "max_tokens": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    e.st.flush().await;
    // Released after the Ledger Entry is written.
    assert_eq!(e.st.reserved(bob), 0);
}

#[tokio::test]
async fn metrics_token_is_required() {
    let e = env().await;
    let r = e
        .http
        .get(format!("{}/metrics", e.proxy))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = e
        .http
        .get(format!("{}/metrics", e.proxy))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // Right token; this test state has no recorder, so 404.
    let r = e
        .http
        .get(format!("{}/metrics", e.proxy))
        .bearer_auth("mtok")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn sessions_survive_restart_and_logout_ends_them() {
    let e = env().await;
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c
        .post(format!("{}/admin/login", e.proxy))
        .form(&[("password", "pw")])
        .send()
        .await
        .unwrap();
    let set = r.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(set.contains("Max-Age=604800"));
    let cookie = set.split(';').next().unwrap().to_owned();
    let token = cookie.split_once('=').unwrap().1.to_owned();

    // A new process on the same database accepts the session.
    let st2 = llm_accounting::init(&e.cfg, None).await.unwrap();
    let app2 = llm_accounting::router(st2.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app2).await.unwrap() });
    let r = c
        .get(format!("http://{addr}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    c.post(format!("http://{addr}/admin/logout"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(
        !llm_accounting::db::session_valid(&e.st.db, &token)
            .await
            .unwrap()
    );
    let r = c
        .get(format!("{}/admin", e.proxy))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
}
