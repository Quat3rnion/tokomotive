use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::json;
use std::time::Duration;
use tokomotive::{config::Config, state::AppState};

struct Env {
    st: AppState,
    url: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Env {
    async fn new(app: Router, options: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg: Config = toml::from_str(&format!(
            r#"
[server]
database='sqlite::memory:'
mode='simple'
admin_password='password123'
upstream_timeout_secs=3
[[providers]]
name='slow'
kind='openai'
base_url='{base}'
api_key='sk-upstream'
{options}
[[users]]
name='alice'
keys=['sk-alice']
"#
        ))
        .unwrap();
        static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
            std::sync::OnceLock::new();
        let st = tokomotive::init(
            &cfg,
            Some(
                METRICS
                    .get_or_init(|| tokomotive::metrics::install().unwrap())
                    .clone(),
            ),
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = tokomotive::router(st.clone());
        let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            st,
            url,
            tasks: vec![upstream, proxy],
        }
    }
    async fn send(&self, stream: bool, messages: bool) -> reqwest::Response {
        reqwest::Client::new().post(format!("{}/v1/{}", self.url, if messages { "messages" } else { "chat/completions" }))
            .bearer_auth("sk-alice").json(&json!({"model":"slow/m", "messages":[{"role":"user","content":"hi"}], "max_tokens":10,"stream":stream}))
            .send().await.unwrap()
    }
}
fn success() -> axum::response::Response {
    Json(json!({"model":"m","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})).into_response()
}

#[tokio::test]
async fn provider_request_timeout_overrides_global() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            success()
        }),
    );
    let env = Env::new(app, "[providers.limits]\nrequest_timeout_ms=100").await;
    let start = std::time::Instant::now();
    let response = env.send(false, false).await;
    assert_eq!(response.status(), 502);
    assert!(start.elapsed() < Duration::from_secs(1));
    response.bytes().await.unwrap();
    env.st.flush().await;
    let attempts = tokomotive::observability::attempts(&env.st.db, 1)
        .await
        .unwrap();
    assert_eq!(attempts[0].error_class, "request_timeout");
}

fn streaming_app(delay: Duration, chunks: usize) -> Router {
    Router::new().route("/v1/chat/completions", post(move || async move {
        let stream = futures_util::stream::unfold(0, move |i| async move {
            if i > chunks { return None; }
            if i > 0 { tokio::time::sleep(delay).await; }
            let data = if i == chunks { "data: [DONE]\n\n" } else { "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n" };
            Some((Ok::<_, std::io::Error>(data), i + 1))
        });
        ([("content-type", "text/event-stream")], axum::body::Body::from_stream(stream))
    }))
}

#[tokio::test]
async fn stalled_stream_sends_error_in_both_client_formats() {
    for messages in [false, true] {
        let env = Env::new(
            streaming_app(Duration::from_secs(2), 2),
            "[providers.limits]\nstream_idle_timeout_ms=100",
        )
        .await;
        let response = env.send(true, messages).await;
        assert_eq!(response.status(), 200);
        let body = tokio::time::timeout(Duration::from_secs(1), response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("stream_idle_timeout"), "{body}");
        if messages {
            assert!(body.contains("event: error\n"));
        }
        assert!(!body.contains("[DONE]") && !body.contains("event: message_stop"));
        env.st.flush().await;
        let (status, estimated, aborted): (i64, bool, bool) =
            sqlx::query_as("SELECT status,estimated,client_aborted FROM ledger")
                .fetch_one(&env.st.db)
                .await
                .unwrap();
        assert_eq!((status, estimated, aborted), (502, true, false));
        assert_eq!(
            tokomotive::observability::attempts(&env.st.db, 1)
                .await
                .unwrap()[0]
                .error_class,
            "stream_idle_timeout"
        );
    }
}

#[tokio::test]
async fn healthy_stream_outlives_request_timeout_when_bytes_keep_arriving() {
    let env = Env::new(
        streaming_app(Duration::from_millis(40), 10),
        "[providers.limits]\nrequest_timeout_ms=100\nstream_idle_timeout_ms=200",
    )
    .await;
    let body = env.send(true, false).await.text().await.unwrap();
    assert!(body.contains("[DONE]"), "{body}");
    assert!(!body.contains("timeout"));
}

fn holding_app() -> Router {
    Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<serde_json::Value>| async move {
            if body["stream"] == true {
                let stream = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                    )
                })
                .chain(futures_util::stream::pending());
                return (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response();
            }
            success()
        }),
    )
}

#[tokio::test]
async fn concurrency_limit_returns_429_then_falls_back_without_circuit_failure() {
    let env = Env::new(
        holding_app(),
        "[providers.limits]\nmax_concurrent=1\nstream_idle_timeout_ms=300",
    )
    .await;
    let holder = env.send(true, false).await;
    for index in 0..4 {
        let r = env.send(false, index == 1).await;
        assert_eq!(r.status(), 429);
        assert!(r.headers().contains_key("retry-after"));
        if index == 1 {
            assert_eq!(
                r.json::<serde_json::Value>().await.unwrap()["type"],
                "error"
            );
        }
    }
    let base: String = sqlx::query_scalar("SELECT base_url FROM providers WHERE name='slow'")
        .fetch_one(&env.st.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO providers(name,kind,base_url,keys_json) VALUES('fast','\"openai\"',?,'[]')",
    )
    .bind(base)
    .execute(&env.st.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO routes(name,config_json) VALUES('quick',?)")
        .bind(json!({"name":"quick","deployments":[{"provider":"fast","model":"m"}]}).to_string())
        .execute(&env.st.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO routes(name,config_json) VALUES('both',?)").bind(json!({"name":"both","deployments":[{"provider":"slow","model":"m"}],"fallbacks":["quick"]}).to_string()).execute(&env.st.db).await.unwrap();
    env.st.reload_settings().await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", env.url))
        .bearer_auth("sk-alice")
        .json(&json!({"model":"both","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-tokomotive-provider"], "fast");
    r.bytes().await.unwrap();
    holder.bytes().await.unwrap();
    let r = env.send(false, false).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    env.st.flush().await;
    let reasons: Vec<String> =
        sqlx::query_scalar("SELECT error_class FROM request_attempts WHERE skip_reason!=''")
            .fetch_all(&env.st.db)
            .await
            .unwrap();
    assert_eq!(reasons, vec!["concurrency_limited"; 5]);
    let metrics = reqwest::get(format!("{}/metrics", env.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for name in [
        "tokomotive_provider_inflight",
        "tokomotive_provider_limited_total",
        "tokomotive_provider_timeouts_total",
    ] {
        assert!(metrics.contains(name), "missing {name}");
    }
}

use futures_util::StreamExt;

#[tokio::test]
async fn retries_back_off_per_provider_and_respect_overall_cap() {
    use std::sync::{Arc, Mutex};
    let times = Arc::new(Mutex::new(Vec::new()));
    let seen = times.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let seen = seen.clone();
            async move {
                let mut times = seen.lock().unwrap();
                times.push(std::time::Instant::now());
                if times.len() < 3 {
                    return axum::http::StatusCode::BAD_GATEWAY.into_response();
                }
                success()
            }
        }),
    );
    let env = Env::new(app, "[providers.limits]\nmax_retries=2\nbackoff_base_ms=60\nbackoff_max_ms=100\nbackoff_jitter=false").await;
    let response = env.send(false, false).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    {
        let times = times.lock().unwrap();
        assert_eq!(times.len(), 3);
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(60));
        assert!(times[2].duration_since(times[1]) >= Duration::from_millis(100));
    }
    let mut settings: serde_json::Value = serde_json::from_str(
        &tokomotive::settings::value(&env.st.db, "runtime_settings")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    settings["router"]["max_attempts"] = json!(1);
    tokomotive::settings::put(&env.st.db, "runtime_settings", &settings.to_string())
        .await
        .unwrap();
    env.st.reload_settings().await.unwrap();
    times.lock().unwrap().clear();
    assert_eq!(env.send(false, false).await.status(), 502);
    assert_eq!(times.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn connect_timeout_bounds_a_stalled_tls_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    let stalled = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let env = Env::new(
        holding_app(),
        "[providers.limits]\nconnect_timeout_ms=80\nrequest_timeout_ms=1500",
    )
    .await;
    sqlx::query("UPDATE providers SET base_url=?")
        .bind(base)
        .execute(&env.st.db)
        .await
        .unwrap();
    env.st.reload_settings().await.unwrap();
    let start = std::time::Instant::now();
    let r = env.send(false, false).await;
    assert_eq!(r.status(), 502);
    r.bytes().await.unwrap();
    assert!(start.elapsed() < Duration::from_millis(1000));
    env.st.flush().await;
    assert_eq!(
        tokomotive::observability::attempts(&env.st.db, 1)
            .await
            .unwrap()[0]
            .error_class,
        "connect_timeout"
    );
    stalled.abort();
}

#[tokio::test]
async fn bounded_wait_admits_after_release_and_rejects_at_deadline() {
    for (wait, expected) in [(40, 429), (600, 200)] {
        let env = Env::new(holding_app(), &format!("[providers.limits]\nmax_concurrent=1\nstream_idle_timeout_ms=200\nmax_wait_ms={wait}")).await;
        let holder = env.send(true, false).await;
        let start = std::time::Instant::now();
        let r = env.send(false, false).await;
        assert_eq!(r.status(), expected);
        assert!(start.elapsed() >= Duration::from_millis(if expected == 429 { 35 } else { 150 }));
        r.bytes().await.unwrap();
        holder.bytes().await.unwrap();
    }
}

#[tokio::test]
async fn live_resize_keeps_old_holders_and_applies_new_admission_limit() {
    let env = Env::new(
        holding_app(),
        "[providers.limits]\nmax_concurrent=1\nstream_idle_timeout_ms=800",
    )
    .await;
    let first = env.send(true, false).await;
    sqlx::query("UPDATE providers SET limits_json=?")
        .bind(json!({"max_concurrent":2,"stream_idle_timeout_ms":1200}).to_string())
        .execute(&env.st.db)
        .await
        .unwrap();
    env.st.reload_settings().await.unwrap();
    let second = env.send(true, false).await;
    assert_eq!(second.status(), 200);
    sqlx::query("UPDATE providers SET limits_json=?")
        .bind(json!({"max_concurrent":1,"stream_idle_timeout_ms":1200}).to_string())
        .execute(&env.st.db)
        .await
        .unwrap();
    env.st.reload_settings().await.unwrap();
    assert_eq!(env.send(false, false).await.status(), 429);
    first.bytes().await.unwrap();
    assert_eq!(env.send(false, false).await.status(), 429);
    // Both old holders finish without cancellation after the limit shrinks.
    assert!(second.text().await.unwrap().contains("stream_idle_timeout"));
    assert_eq!(env.send(false, false).await.status(), 200);
}

#[tokio::test]
async fn request_timeout_includes_a_stalled_nonstream_body() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let stream =
                futures_util::stream::once(async { Ok::<_, std::io::Error>("{\"model\":") })
                    .chain(futures_util::stream::pending());
            (
                [("content-type", "application/json")],
                axum::body::Body::from_stream(stream),
            )
        }),
    );
    let env = Env::new(app, "[providers.limits]\nrequest_timeout_ms=80").await;
    let r = env.send(false, false).await;
    assert_eq!(r.status(), 502);
    r.bytes().await.unwrap();
    env.st.flush().await;
    assert_eq!(
        tokomotive::observability::attempts(&env.st.db, 1)
            .await
            .unwrap()[0]
            .error_class,
        "request_timeout"
    );
}

#[tokio::test]
async fn provider_card_saves_overrides_and_blank_inherits_live_server_defaults() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            success()
        }),
    );
    let env = Env::new(app, "").await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let login = client
        .post(format!("{}/admin/login", env.url))
        .form(&[("password", "password123")])
        .send()
        .await
        .unwrap();
    let cookie = login
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
    let page = client
        .get(format!("{}/admin/providers", env.url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Advanced request limits"));
    let base: String = sqlx::query_scalar("SELECT base_url FROM providers WHERE name='slow'")
        .fetch_one(&env.st.db)
        .await
        .unwrap();
    let save = |timeout: &'static str| {
        client
            .post(format!("{}/admin/providers/slow", env.url))
            .header("cookie", &cookie)
            .form(&[
                ("name", "slow"),
                ("kind", "openai"),
                ("base_url", base.as_str()),
                ("request_timeout_ms", timeout),
                ("max_retries", "0"),
            ])
    };
    assert_eq!(save("50").send().await.unwrap().status(), 303);
    assert_eq!(env.send(false, false).await.status(), 502);
    let response = client
        .post(format!("{}/admin/settings", env.url))
        .header("cookie", &cookie)
        .form(&[
            ("upstream_timeout_secs", "3"),
            ("session_days", "7"),
            ("failure_threshold", "3"),
            ("cooldown_secs", "0"),
            ("max_attempts", "8"),
            ("request_timeout_ms", "70"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    assert_eq!(save("").send().await.unwrap().status(), 303);
    assert_eq!(env.send(false, false).await.status(), 502);
    let saved: String = sqlx::query_scalar("SELECT limits_json FROM providers WHERE name='slow'")
        .fetch_one(&env.st.db)
        .await
        .unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&saved).unwrap()["request_timeout_ms"].is_null()
    );
}

#[tokio::test]
async fn skipped_keys_do_not_consume_the_provider_retry_allowance() {
    let env = Env::new(
        Router::new().route("/v1/chat/completions", post(|| async { success() })),
        "api_keys=['sk-good']\n[providers.limits]\nmax_retries=0",
    )
    .await;
    tokomotive::quota::record(
        &env.st.db,
        "slow",
        &tokomotive::quota::key_id("sk-upstream"),
        401,
        &axum::http::HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    assert_eq!(env.send(false, false).await.status(), 200);
}
