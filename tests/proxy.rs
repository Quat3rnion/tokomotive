//! End-to-end tests: a mock Provider, the real Proxy, the real database.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tokomotive::config::Config;
use tokomotive::state::AppState;

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
    uri: axum::http::Uri,
    mut headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    headers.insert("x-seen-uri", uri.to_string().parse().unwrap());
    seen.0.lock().unwrap().push((headers.clone(), b.clone()));
    if headers.contains_key("x-mock-error") {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(
                json!({"error":{"type":"invalid_request_error","message":"mock rejected request"}}),
            ),
        )
            .into_response();
    }
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
    if b["metadata"]["omit_usage"] == json!(true) {
        return Json(json!({"model": b["model"], "choices": []})).into_response();
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
    Json(json!({"id":"chat_test", "model": b["model"], "choices": [{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}], "usage": usage})).into_response()
}

async fn mock_anthropic(
    axum::extract::State(seen): axum::extract::State<Seen>,
    uri: axum::http::Uri,
    mut headers: HeaderMap,
    Json(b): Json<Value>,
) -> Response {
    headers.insert("x-seen-uri", uri.to_string().parse().unwrap());
    seen.0.lock().unwrap().push((headers.clone(), b.clone()));
    if headers.contains_key("x-mock-error") {
        return (axum::http::StatusCode::BAD_REQUEST,Json(json!({"type":"error","error":{"type":"invalid_request_error","message":"mock rejected request"}}))).into_response();
    }
    let start_usage = json!({"input_tokens": 50, "output_tokens": 1, "cache_read_input_tokens": 2000,
        "cache_creation_input_tokens": 300, "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200}});
    if b["stream"] == json!(true) {
        let slow = b["metadata"]["slow"] == json!(true) || headers.contains_key("x-mock-slow");
        let mut ev = vec![format!(
            "event: message_start\ndata: {}\n\n",
            json!({"type": "message_start", "message": {"model": "claude-sonnet-4-5-20250929", "usage": start_usage}})
        )];
        ev.push(format!("event: content_block_start\ndata: {}\n\n", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})));
        for _ in 0..(if slow { 20 } else { 2 }) {
            ev.push(format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type": "content_block_delta", "index":0, "delta": {"type":"text_delta", "text": "x"}})
            ));
        }
        ev.push(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
                .into(),
        );
        ev.push(format!(
            "event: message_delta\ndata: {}\n\n",
            json!({"type": "message_delta", "delta":{"stop_reason":"end_turn"}, "usage": {"output_tokens": 77}})
        ));
        ev.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into());
        return sse(ev, if slow { 40 } else { 2 });
    }
    let mut u = start_usage;
    u["output_tokens"] = json!(77);
    if b["tools"].is_array() {
        return Json(json!({"id":"msg_tool", "type":"message","role":"assistant","model":b["model"],"content":[{"type":"tool_use","id":"call_1","name":"weather","input":{"city":"Paris"}}],"stop_reason":"tool_use","usage":u})).into_response();
    }
    Json(json!({"id":"msg_test", "type":"message", "role":"assistant", "content":[{"type":"text","text":"hi"}], "stop_reason":"end_turn", "model": "claude-sonnet-4-5-20250929", "usage": u})).into_response()
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

async fn set_mode(e: &Env, mode: &str) {
    let settings = e.st.settings();
    tokomotive::db::delete_session(&e.st.db, "mode-test")
        .await
        .unwrap();
    tokomotive::db::create_session(&e.st.db, "mode-test", 1)
        .await
        .unwrap();
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}/admin/settings", e.proxy))
        .header("cookie", "tokomotive_session=mode-test")
        .form(&[
            (
                "upstream_timeout_secs",
                settings.upstream_timeout_secs.to_string(),
            ),
            ("session_days", settings.session_days.to_string()),
            (
                "failure_threshold",
                settings.router.failure_threshold.to_string(),
            ),
            ("cooldown_secs", settings.router.cooldown_secs.to_string()),
            ("max_attempts", settings.router.max_attempts.to_string()),
            ("mode", mode.to_owned()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
}

#[tokio::test]
async fn simple_mode_skips_budget_keeps_usage_and_full_mode_enforces_live() {
    let e = env().await;
    set_mode(&e, "simple").await;
    let request = || {
        e.http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth("sk-bob")
            .json(&json!({"model":"gpt-4o", "max_tokens":1_000_000}))
    };
    let response = request().send().await.unwrap();
    assert_eq!(response.status(), 200);
    let row = last_entry(&e.st).await;
    assert_eq!(
        (i(&row, "input"), i(&row, "output"), i(&row, "cache_read")),
        (600, 100, 400)
    );
    assert_eq!(
        (i(&row, "cost_micros"), i(&row, "price_micros")),
        (3000, 3600)
    );
    assert_eq!(e.st.reserved(row.get("user_id")), 0);
    set_mode(&e, "full").await;
    assert_eq!(request().send().await.unwrap().status(), 402);
    assert_eq!(e.seen.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn simple_mode_dashboard_shows_usage_without_financial_controls() {
    let e = env().await;
    set_mode(&e, "simple").await;
    e.http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-alice")
        .json(&json!({"model":"gpt-4o"}))
        .send()
        .await
        .unwrap();
    for body in [
        json!({"model":"gpt-pool"}),
        json!({"model":"new-model", "metadata":{"omit_usage":true}}),
    ] {
        assert_eq!(
            e.http
                .post(format!("{}/v1/chat/completions", e.proxy))
                .bearer_auth("sk-alice")
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    e.st.flush().await;
    assert_eq!(tokomotive::db::count_flags(&e.st.db).await.unwrap(), (1, 1));
    let providers = tokomotive::db::by_provider(&e.st.db, 0).await.unwrap();
    let pool = providers.iter().find(|g| g.key == "pool").unwrap();
    assert_eq!(
        (
            pool.t.requests,
            pool.t.input,
            pool.t.output,
            pool.t.cache_read,
            pool.attempts,
            pool.retried
        ),
        (1, 600, 100, 400, 2, 1)
    );
    for path in [
        "/admin",
        "/admin/ledger",
        "/admin/ledger?filter=unpriced",
        "/admin/users",
        "/admin/keys",
        "/admin/models",
        "/admin/models/1",
        "/admin/settings",
        "/admin/teams",
        "/admin/providers",
        "/admin/routes",
    ] {
        let response = e
            .http
            .get(format!("{}{path}", e.proxy))
            .header("cookie", "tokomotive_session=mode-test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let html = response.text().await.unwrap();
        for hidden in [
            "Cost",
            "Price",
            "Profit",
            "Budget",
            "Rate Limits",
            "RPM",
            "TPM",
            "unpriced",
            "estimated",
            "name=\"default_markup_percent\"",
            "name=\"reserve_output_tokens\"",
        ] {
            assert!(!html.contains(hidden), "{path} contains {hidden}");
        }
        if path == "/admin" || path == "/admin/ledger" {
            assert!(html.contains("600") && html.contains("400") && html.contains("100"));
            assert!(html.contains("Cache write 5m") && html.contains("Cache write 1h"));
            assert!(html.contains("Attempts"));
        }
        if path == "/admin" {
            assert!(html.contains("By Provider") && html.contains("Error rate"));
        }
    }
    set_mode(&e, "full").await;
    let html = e
        .http
        .get(format!("{}/admin", e.proxy))
        .header("cookie", "tokomotive_session=mode-test")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Price") && html.contains("Cost") && html.contains("Profit"));
    assert!(html.contains("unpriced") && html.contains("estimated"));
}

#[tokio::test]
async fn simple_mode_skips_key_and_user_limits_without_counting_admissions_or_tokens() {
    use tokomotive::{db, ratelimit::Limits};
    let metrics = tokomotive::metrics::install().unwrap();
    let e = env().await;
    let blocked = Limits {
        rpm: Some(0),
        tpm: Some(0),
    };
    let user =
        db::create_user_with_limits(&e.st.db, "simple-mode-metrics", None, None, false, blocked)
            .await
            .unwrap();
    let (key_id, key) = db::create_key_with_limits(
        &e.st.db,
        user,
        "",
        Some(vec!["claude-sonnet-4-5".into(), "gpt-image-2".into()]),
        blocked,
    )
    .await
    .unwrap();
    set_mode(&e, "simple").await;
    for (path, body, expected) in [
        (
            "messages",
            json!({"model":"claude-sonnet-4-5", "messages":[], "stream":true}),
            200,
        ),
        (
            "images/generations",
            json!({"model":"gpt-image-2", "prompt":"x"}),
            200,
        ),
        ("chat/completions", json!({"model":"gpt-4o"}), 403),
    ] {
        let r = e
            .http
            .post(format!("{}/v1/{path}", e.proxy))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), expected);
        r.bytes().await.unwrap();
    }
    // The streamed request can be recorded after the image request, so
    // select the image entry by endpoint, not by order.
    e.st.flush().await;
    let row = sqlx::query("SELECT * FROM ledger WHERE endpoint = 'images'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        (i(&row, "image_input"), i(&row, "image_output")),
        (200, 1000)
    );
    let totals = db::totals_since(&e.st.db, 0).await.unwrap();
    assert_eq!(
        (
            totals.requests,
            totals.cache_write_5m,
            totals.cache_write_1h
        ),
        (2, 100, 200)
    );
    let rendered = metrics.render();
    for metric in [
        "llm_tokens_total",
        "llm_request_duration_seconds",
        "llm_time_to_first_token_seconds",
    ] {
        assert!(
            rendered
                .lines()
                .any(|line| line.starts_with(metric)
                    && line.contains("user=\"simple-mode-metrics\"")),
            "{metric}"
        );
    }
    set_mode(&e, "full").await;
    let request = || {
        e.http
            .post(format!("{}/v1/messages", e.proxy))
            .bearer_auth(&key)
            .json(&json!({"model":"claude-sonnet-4-5", "messages":[]}))
    };
    assert_eq!(request().send().await.unwrap().status(), 429);
    let limited = Limits {
        rpm: Some(1),
        tpm: Some(1),
    };
    db::update_user_with_limits(&e.st.db, user, None, None, false, false, limited)
        .await
        .unwrap();
    db::update_key_limits(&e.st.db, key_id, limited)
        .await
        .unwrap();
    assert_eq!(request().send().await.unwrap().status(), 200);
    e.st.flush().await;
    assert_eq!(request().send().await.unwrap().status(), 429);
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
admin_password_env = "TOKOMOTIVE_TEST_UNSET_VAR"
metrics_token = "mtok"
metrics_token_env = "TOKOMOTIVE_TEST_UNSET_VAR"
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
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let app = tokomotive::router(st.clone());
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
    for (mode, attempts) in [("full", 3), ("simple", 1)] {
        set_mode(&e, mode).await;
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
async fn mixed_route_serves_both_client_formats_in_deployment_order() {
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
    for endpoint in ["chat/completions", "messages"] {
        let r = e
            .http
            .post(format!("{}/v1/{endpoint}", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model": "smart", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        if endpoint == "chat/completions" {
            assert_eq!(body["choices"][0]["message"]["content"], "hi");
        } else {
            assert_eq!(body["content"][0]["text"], "hi");
        }
        let row = last_entry(&e.st).await;
        assert_eq!(i(&row, "attempts"), 1);
        assert_eq!(i(&row, "input"), 50);
        assert_eq!(i(&row, "output"), 77);
        assert_eq!(i(&row, "cost_micros"), 3480);
        assert_eq!(row.get::<String, _>("provider"), "anthropic");
    }
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1["model"], "claude-sonnet-4-5");
    assert_eq!(seen[0].0["x-api-key"], "upstream-anthropic-key");
    assert_eq!(seen[0].0["anthropic-version"], "2023-06-01");
    assert!(!seen[0].0.contains_key("authorization"));
}

#[tokio::test]
async fn translated_streams_keep_upstream_usage_and_client_formats() {
    let e = env().await;
    for (endpoint, model, include_usage) in [
        ("chat/completions", "claude-sonnet-4-5", false),
        ("chat/completions", "claude-sonnet-4-5", true),
        ("messages", "gpt-4o", false),
    ] {
        let r = e.http.post(format!("{}/v1/{endpoint}", e.proxy))
            .bearer_auth("sk-carol")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "test-beta")
            .json(&json!({"model":model,"messages":[],"max_tokens":100,"stream":true,"stream_options":{"include_usage":include_usage}}))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        assert!(!r.headers().contains_key("content-length"));
        let text = r.text().await.unwrap();
        let row = last_entry(&e.st).await;
        assert_eq!(i(&row, "estimated"), 0);
        if endpoint == "chat/completions" {
            assert!(text.contains("chat.completion.chunk"), "{text}");
            assert!(text.contains("[DONE]"));
            assert_eq!(text.contains("prompt_tokens"), include_usage);
            assert_eq!(i(&row, "input"), 50);
            assert_eq!(i(&row, "output"), 77);
            assert_eq!(i(&row, "cost_micros"), 3480);
        } else {
            assert!(text.contains("event: message_start"), "{text}");
            assert!(text.contains("event: message_stop"));
            assert!(!text.contains("[DONE]"));
            assert_eq!(i(&row, "input"), 600);
            assert_eq!(i(&row, "cache_read"), 400);
            assert_eq!(i(&row, "output"), 100);
            assert_eq!(i(&row, "cost_micros"), 3000);
        }
    }
    let seen = e.seen.0.lock().unwrap();
    assert!(!seen[2].0.contains_key("anthropic-version"));
    assert!(!seen[2].0.contains_key("anthropic-beta"));
    assert_eq!(seen[2].1["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn key_rpm_limit_returns_retry_after_without_counting_model_reads() {
    use tokomotive::{db, ratelimit::Limits};
    for endpoint in ["chat/completions", "messages"] {
        let e = env().await;
        let auth = db::lookup_key(&e.st.db, "sk-carol").await.unwrap().unwrap();
        db::update_key_limits(
            &e.st.db,
            auth.key_id,
            Limits {
                rpm: Some(1),
                tpm: None,
            },
        )
        .await
        .unwrap();
        for _ in 0..2 {
            assert_eq!(
                e.http
                    .get(format!("{}/v1/models", e.proxy))
                    .bearer_auth("sk-carol")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        for status in [200, 429] {
            let r = e.http.post(format!("{}/v1/{endpoint}",e.proxy)).bearer_auth("sk-carol")
                .json(&json!({"model":if endpoint=="messages" {"claude-sonnet-4-5"} else {"gpt-4o"},"messages":[]}))
                .send().await.unwrap();
            assert_eq!(r.status(), status);
            if status == 429 {
                assert!(
                    (1..=60).contains(
                        &r.headers()["retry-after"]
                            .to_str()
                            .unwrap()
                            .parse::<u64>()
                            .unwrap()
                    )
                );
                let body: Value = r.json().await.unwrap();
                assert_eq!(body["error"]["type"], "rate_limit_error");
                assert_eq!(body["type"] == "error", endpoint == "messages");
            }
        }
        assert_eq!(e.seen.0.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn tpm_limit_counts_all_upstream_token_classes_after_completion() {
    use tokomotive::{db, ratelimit::Limits};
    for stream in [false, true] {
        let e = env().await;
        let auth = db::lookup_key(&e.st.db, "sk-carol").await.unwrap().unwrap();
        // Anthropic uses 2427 tokens across all exclusive classes, only 127
        // of which are uncached input and output.
        db::update_key_limits(
            &e.st.db,
            auth.key_id,
            Limits {
                rpm: None,
                tpm: Some(2400),
            },
        )
        .await
        .unwrap();
        for status in [200, 429] {
            let r = e
                .http
                .post(format!("{}/v1/chat/completions", e.proxy))
                .bearer_auth("sk-carol")
                .json(&json!({"model":"claude-sonnet-4-5","messages":[],"stream":stream}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), status);
            r.bytes().await.unwrap();
            e.st.flush().await;
        }
        assert_eq!(e.seen.0.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn messages_client_uses_openai_nonstream_and_keeps_query_and_usage() {
    let e = env().await;
    let r=e.http.post(format!("{}/v1/messages?test=one%20two",e.proxy)).header("x-api-key","sk-carol")
        .header("anthropic-version","2023-06-01").header("anthropic-beta","example")
        .json(&json!({"model":"openai/gpt-4o","system":"Be brief","messages":[{"role":"user","content":"Hello"}],"max_tokens":100}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/json");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"], json!([{"type":"text","text":"hi"}]));
    assert_eq!(
        body["usage"],
        json!({"input_tokens":600,"cache_read_input_tokens":400,"output_tokens":100})
    );
    let row = last_entry(&e.st).await;
    assert_eq!(i(&row, "cost_micros"), 3000);
    assert_eq!(row.get::<String, _>("endpoint"), "messages");
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(
        seen[0].0["x-seen-uri"],
        "/v1/chat/completions?test=one%20two"
    );
    assert!(!seen[0].0.contains_key("x-api-key"));
    assert!(!seen[0].0.contains_key("anthropic-version"));
    assert!(!seen[0].0.contains_key("anthropic-beta"));
    assert_eq!(seen[0].0["authorization"], "Bearer upstream-openai-key");
    assert_eq!(seen[0].1["model"], "gpt-4o");
    assert_eq!(
        seen[0].1["messages"][0],
        json!({"role":"system","content":"Be brief"})
    );
}

#[tokio::test]
async fn chat_tool_call_round_trip_uses_anthropic_blocks() {
    let e = env().await;
    let mut body = json!({"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"Weather?"}],"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]});
    let r = e
        .http
        .post(format!("{}/v1/chat/completions?trace=yes", e.proxy))
        .bearer_auth("sk-carol")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let response: Value = r.json().await.unwrap();
    assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
    let assistant = response["choices"][0]["message"].clone();
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "weather");
    let args: Value = serde_json::from_str(
        assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args, json!({"city":"Paris"}));
    body["messages"].as_array_mut().unwrap().extend([
        assistant,
        json!({"role":"tool","tool_call_id":"call_1","content":"Sunny"}),
    ]);
    body.as_object_mut().unwrap().remove("tools");
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.proxy))
        .bearer_auth("sk-carol")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "hi"
    );
    e.st.flush().await;
    let seen = e.seen.0.lock().unwrap();
    assert_eq!(seen[0].0["x-seen-uri"], "/v1/messages?trace=yes");
    assert_eq!(seen[0].1["tools"][0]["name"], "weather");
    assert_eq!(
        seen[1].1["messages"][1]["content"][0],
        json!({"type":"tool_use","id":"call_1","name":"weather","input":{"city":"Paris"}})
    );
    assert_eq!(
        seen[1].1["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(
        seen[1].1["messages"][2]["content"][0]["tool_use_id"],
        "call_1"
    );
}

#[tokio::test]
async fn translated_errors_use_client_format_and_invalid_requests_never_reach_upstream() {
    let e = env().await;
    for (endpoint, model) in [
        ("chat/completions", "claude-sonnet-4-5"),
        ("messages", "gpt-4o"),
    ] {
        let r = e
            .http
            .post(format!("{}/v1/{endpoint}", e.proxy))
            .bearer_auth("sk-carol")
            .header("x-mock-error", "yes")
            .json(&json!({"model":model,"messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "mock rejected request");
        assert_eq!(body["type"] == "error", endpoint == "messages");
        let row = last_entry(&e.st).await;
        assert_eq!(i(&row, "status"), 400);
        assert_eq!(i(&row, "cost_micros"), 0);
        assert_eq!(i(&row, "unpriced"), 0);
        let count = e.seen.0.lock().unwrap().len();
        let r = e
            .http
            .post(format!("{}/v1/{endpoint}", e.proxy))
            .bearer_auth("sk-carol")
            .json(&json!({"model":model,"messages":[{"role":"invalid","content":"x"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["type"] == "error", endpoint == "messages");
        assert_eq!(e.seen.0.lock().unwrap().len(), count);
    }
}

#[tokio::test]
async fn user_rate_limit_is_shared_by_two_virtual_keys() {
    use tokomotive::{db, ratelimit::Limits};
    for limits in [
        Limits {
            rpm: Some(1),
            tpm: None,
        },
        Limits {
            rpm: None,
            tpm: Some(1100),
        },
    ] {
        let e = env().await;
        let auth = db::lookup_key(&e.st.db, "sk-carol").await.unwrap().unwrap();
        db::update_user_with_limits(&e.st.db, auth.user_id, None, None, false, false, limits)
            .await
            .unwrap();
        let (_, second) = db::create_key(&e.st.db, auth.user_id, "second", None)
            .await
            .unwrap();
        for (key, status) in [("sk-carol", 200), (second.as_str(), 429)] {
            let r = e
                .http
                .post(format!("{}/v1/chat/completions", e.proxy))
                .bearer_auth(key)
                .json(&json!({"model":"gpt-4o"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), status);
            r.bytes().await.unwrap();
            e.st.flush().await;
        }
        assert_eq!(e.seen.0.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn authentication_and_allowlist_checks_do_not_consume_rate_limits() {
    use tokomotive::{db, ratelimit::Limits};
    let e = env().await;
    let auth = db::lookup_key(&e.st.db, "sk-carol").await.unwrap().unwrap();
    let (_, key) = db::create_key_with_limits(
        &e.st.db,
        auth.user_id,
        "restricted",
        Some(vec!["gpt-4o".into()]),
        Limits {
            rpm: Some(1),
            tpm: None,
        },
    )
    .await
    .unwrap();
    for (credential, model, status) in [
        ("invalid", "gpt-4o", 401),
        (key.as_str(), "claude-sonnet-4-5", 403),
        (key.as_str(), "gpt-4o", 200),
        (key.as_str(), "claude-sonnet-4-5", 403),
        (key.as_str(), "gpt-4o", 429),
    ] {
        let r = e
            .http
            .post(format!("{}/v1/chat/completions", e.proxy))
            .bearer_auth(credential)
            .json(&json!({"model":model}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status);
    }
    assert_eq!(e.seen.0.lock().unwrap().len(), 1);
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
        tokomotive::db::create_key(&e.st.db, carol, "routes", Some(vec!["smart".into()]))
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
            tokomotive::db::create_key(&e.st.db, carol, "prefix", Some(vec![allowed.into()]))
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
        e.st.set_router_table(tokomotive::routing::RouterTable::from_config(&e.cfg));
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
    let groups = tokomotive::db::by_model(&e.st.db, 0).await.unwrap();
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
    for endpoint in ["messages", "chat/completions"] {
        let e = env().await;
        let r = e
        .http
        .post(format!("{}/v1/{endpoint}", e.proxy))
        .header("x-api-key", "sk-carol")
        .header("x-mock-slow", "yes")
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
        assert_eq!(i(&row, "cost_micros"), 3480);
    }
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
    let (_, key) =
        tokomotive::db::create_key(&e.st.db, alice, "t", Some(vec!["claude-sonnet-4-5".into()]))
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
    tokomotive::db::revoke_key(&e.st.db, kid).await.unwrap();
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
    set_mode(&e, "simple").await;
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
    let before = tokomotive::db::totals_since(&e.st.db, 0).await.unwrap();
    assert_eq!(before.cost_micros, 0);

    // Operator adds the card; reprice.
    let id = tokomotive::db::create_model(&e.st.db, "gpt-9", "openai", &[])
        .await
        .unwrap();
    let card = tokomotive::pricing::CardRates {
        base: tokomotive::pricing::Rates {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        },
        tiers: vec![],
    };
    tokomotive::db::add_cost_card(&e.st.db, id, &card)
        .await
        .unwrap();
    e.st.reload_catalog().await.unwrap();
    set_mode(&e, "full").await;
    let n = tokomotive::db::reprice_unpriced(&e.st.db, &e.st.catalog())
        .await
        .unwrap();
    assert_eq!(n, 1);
    let row = last_entry(&e.st).await;
    assert_eq!((i(&row, "unpriced"), i(&row, "cost_micros")), (0, 1100));
    // Rollups follow the ledger.
    let after = tokomotive::db::totals_since(&e.st.db, 0).await.unwrap();
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
        assert!(body.contains("Tokomotive"), "{path}");
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
    assert!(r.text().await.unwrap().contains("sk-toko-"));

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
        Some((_, tokomotive::pricing::PriceSpec::MarkupBp(3000)))
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
    let st2 = tokomotive::init(&e.cfg, None).await.unwrap();
    let app2 = tokomotive::router(st2.clone());
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
        !tokomotive::db::session_valid(&e.st.db, &token)
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
