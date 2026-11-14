use axum::{Json, http::HeaderMap, routing::post};
use serde_json::{Value, json};

async fn count_mock(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(headers["x-api-key"], "private-key");
    assert_eq!(body["model"], "upstream-model");
    assert!(body.get("max_tokens").is_none());
    Json(json!({"input_tokens":42,"usage":{"input_tokens":42,"output_tokens":1}}))
}

struct Harness {
    _dir: tempfile::TempDir,
    state: tokomotive::state::AppState,
    url: String,
    client: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn new(kind: &str, extra: &str, upstream: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let cfg = toml::from_str(&format!(
            r#"
[server]
database="sqlite://{}/db.sqlite"
[[providers]]
name="upstream"
kind="{kind}"
base_url="http://{address}"
api_key="private-key"
[[routes]]
name="route"
deployments=[{{provider="upstream",model="upstream-model"}}]
[[users]]
name="alice"
keys=["sk-test"]
{extra}
"#,
            dir.path().display()
        ))
        .unwrap();
        static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
            std::sync::OnceLock::new();
        let metrics = METRICS
            .get_or_init(|| tokomotive::metrics::install().unwrap())
            .clone();
        let state = tokomotive::init(&cfg, Some(metrics)).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = tokomotive::router(state.clone());
        let proxy = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            _dir: dir,
            state,
            url,
            client: reqwest::Client::new(),
            tasks: vec![task, proxy],
        }
    }

    fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth("sk-test")
            .json(body)
    }

    fn audio(&self, path: &str, model: &str, data: &str) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.url)).bearer_auth("sk-test")
            .header("content-type", "multipart/form-data; boundary=test-boundary")
            .body(format!("--test-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\n{data}\r\n--test-boundary--\r\n"))
    }
}

#[tokio::test]
async fn audio_uploads_route_multipart_and_charge_reported_seconds() {
    async fn mock(headers: HeaderMap, body: axum::body::Bytes) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer private-key");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("multipart/form-data; boundary=")
        );
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("upstream-model"));
        assert!(!text.contains("\r\nroute\r\n"));
        assert!(text.contains("RIFF-audio-data"));
        Json(json!({"text":"hello","duration":2.5}))
    }
    let h = Harness::new(
        "openai",
        r#"
[[models]]
name="upstream-model"
provider="upstream"
cost={service_units=100.0}
"#,
        axum::Router::new()
            .route("/v1/audio/transcriptions", post(mock))
            .route("/v1/audio/translations", post(mock)),
    )
    .await;
    for path in ["/v1/audio/transcriptions", "/v1/audio/translations"] {
        let response = h
            .audio(path, "route", "RIFF-audio-data")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.json::<Value>().await.unwrap()["text"], "hello");
    }
    h.state.flush().await;
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT service_units,cost_micros FROM ledger ORDER BY id")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(rows, vec![(3, 300), (3, 300)]);
}

#[tokio::test]
async fn speech_streams_binary_before_completion_and_charges_characters() {
    use axum::response::IntoResponse;
    use futures_util::StreamExt;
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let upstream_release = release.clone();
    let upstream = axum::Router::new().route(
        "/v1/audio/speech",
        post(move |Json(body): Json<Value>| {
            let release = upstream_release.clone();
            async move {
                assert_eq!(body["model"], "upstream-model");
                let chunks = futures_util::stream::once(async { Ok::<_, std::io::Error>("RIFF") })
                    .chain(futures_util::stream::once(async move {
                        release.notified().await;
                        Ok("binary")
                    }));
                (
                    [("content-type", "audio/wav")],
                    axum::body::Body::from_stream(chunks),
                )
                    .into_response()
            }
        }),
    );
    let h = Harness::new(
        "openai",
        r#"
[[models]]
name="upstream-model"
provider="upstream"
cost={service_units=10.0}
"#,
        upstream,
    )
    .await;
    let mut response = h
        .post(
            "/v1/audio/speech",
            &json!({"model":"route","input":"a\u{e9}\u{1f340}","voice":"alloy"}),
        )
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "audio/wav");
    assert_eq!(response.chunk().await.unwrap().unwrap(), "RIFF");
    release.notify_one();
    assert_eq!(response.bytes().await.unwrap(), "binary");
    h.state.flush().await;
    let entry: (String, i64, i64, i64) =
        sqlx::query_as("SELECT endpoint,service_units,cost_micros,streamed FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(entry, ("audio_speech".into(), 3, 30, 1));
}

#[tokio::test]
async fn audio_upload_limit_is_configurable_and_errors_use_proxy_format() {
    let h = Harness::new(
        "openai",
        "",
        axum::Router::new().route(
            "/v1/audio/transcriptions",
            post(|| async { Json(json!({"text":"ok"})) }),
        ),
    )
    .await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.audio_upload_limit_mib',1) WHERE key='runtime_settings'").execute(&h.state.db).await.unwrap();
    h.state.reload_settings().await.unwrap();
    let response = h
        .audio(
            "/v1/audio/transcriptions",
            "route",
            &"a".repeat(1024 * 1024),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
}

#[tokio::test]
async fn audio_endpoints_enforce_model_provider_rpm_and_budget() {
    let h = Harness::new("openai", "", axum::Router::new()).await;
    for path in [
        "/v1/audio/transcriptions",
        "/v1/audio/translations",
        "/v1/audio/speech",
    ] {
        let request = || {
            if path.ends_with("speech") {
                h.post(
                    path,
                    &json!({"model":"route","input":"hello","voice":"alloy"}),
                )
            } else {
                h.audio(path, "route", "audio")
            }
        };
        sqlx::query("UPDATE virtual_keys SET allowed_models='[\"other\"]',allowed_providers='[]',rpm_limit=NULL").execute(&h.state.db).await.unwrap();
        assert_eq!(request().send().await.unwrap().status(), 403);
        sqlx::query("UPDATE virtual_keys SET allowed_models='[]',allowed_providers='[\"other\"]'")
            .execute(&h.state.db)
            .await
            .unwrap();
        assert_eq!(request().send().await.unwrap().status(), 403);
        sqlx::query("UPDATE virtual_keys SET allowed_providers='[]',rpm_limit=0")
            .execute(&h.state.db)
            .await
            .unwrap();
        assert_eq!(request().send().await.unwrap().status(), 429);
    }
    let h = Harness::new(
        "openai",
        r#"
budget_usd=0.01
[[models]]
name="upstream-model"
provider="upstream"
cost={service_units=1000000.0}
"#,
        axum::Router::new(),
    )
    .await;
    for path in ["/v1/audio/transcriptions", "/v1/audio/translations"] {
        assert_eq!(
            h.audio(path, "route", "audio")
                .send()
                .await
                .unwrap()
                .status(),
            402
        );
    }
    assert_eq!(
        h.post(
            "/v1/audio/speech",
            &json!({"model":"route","input":"hello"})
        )
        .send()
        .await
        .unwrap()
        .status(),
        402
    );
    let response = h
        .post(
            "/v1/audio/speech",
            &json!({"model":"route","input":"x".repeat(1024*1024)}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
    assert_eq!(
        h.post("/v1/audio/transcriptions", &json!({"model":"route"}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn audio_uses_seconds_before_duration_and_marks_missing_duration_estimated() {
    let h = Harness::new(
        "openai",
        "key_rpm_limit=2\nkey_tpm_limit=1",
        axum::Router::new()
            .route(
                "/v1/audio/transcriptions",
                post(|| async { Json(json!({"text":"hi","usage":{"seconds":7},"duration":2.0})) }),
            )
            .route("/v1/audio/translations", post(|| async { "hello" })),
    )
    .await;
    assert_eq!(
        h.audio("/v1/audio/transcriptions", "route", "audio")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.audio("/v1/audio/translations", "route", &"a".repeat(16_001))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "hello"
    );
    h.state.flush().await;
    let entries: Vec<(i64, bool)> =
        sqlx::query_as("SELECT service_units,estimated FROM ledger ORDER BY id")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(entries, vec![(7, false), (2, true)]);
    assert_eq!(
        h.audio("/v1/audio/translations", "route", "a")
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
}

#[tokio::test]
async fn extra_endpoint_upstream_errors_have_proxy_json_format() {
    let h = Harness::new(
        "cohere",
        "",
        axum::Router::new().route(
            "/v2/rerank",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"message":"provider error"})),
                )
            }),
        ),
    )
    .await;
    let response = h
        .post(
            "/v1/rerank",
            &json!({"model":"route","query":"q","documents":["a"]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
    let h = Harness::new(
        "openai",
        "",
        axum::Router::new().route(
            "/v1/audio/speech",
            post(|| async { (axum::http::StatusCode::BAD_REQUEST, "provider error") }),
        ),
    )
    .await;
    let response = h
        .post(
            "/v1/audio/speech",
            &json!({"model":"route","input":"hello"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
    h.state.flush().await;
    let units: i64 = sqlx::query_scalar("SELECT service_units FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(units, 0);
}

impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[tokio::test]
async fn rerank_preserves_cohere_body_and_charges_search_units() {
    async fn mock(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer private-key");
        assert_eq!(body["model"], "upstream-model");
        assert_eq!(body["query"], "weather");
        assert_eq!(body["documents"], json!(["sun", "rain"]));
        Json(
            json!({"results":[{"index":0,"relevance_score":0.9}],"meta":{"billed_units":{"search_units":3}}}),
        )
    }
    let h = Harness::new(
        "cohere",
        r#"
[[models]]
name="upstream-model"
provider="upstream"
cost={service_units=2000.0}
price={markup_percent=50.0}
"#,
        axum::Router::new().route("/v2/rerank", post(mock)),
    )
    .await;
    let response = h
        .post(
            "/v1/rerank",
            &json!({"model":"route","query":"weather","documents":["sun","rain"]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["results"][0]["index"],
        0
    );
    h.state.flush().await;
    let entry: (String, i64, i64, i64, i64) =
        sqlx::query_as("SELECT endpoint,service_units,input,cost_micros,price_micros FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(entry, ("rerank".into(), 3, 0, 6000, 9000));
    let metrics = h
        .client
        .get(format!("{}/metrics", h.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("llm_service_units_total"));
    assert!(metrics.contains("endpoint=\"rerank\""));
}

#[tokio::test]
async fn rerank_openai_compatible_reports_tokens_and_enforces_limits() {
    async fn mock(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["model"], "upstream-model");
        Json(json!({"results":[],"usage":{"total_tokens":50}}))
    }
    let h = Harness::new(
        "openai",
        "key_rpm_limit=1",
        axum::Router::new().route("/v1/rerank", post(mock)),
    )
    .await;
    let body = json!({"model":"route","query":"q","documents":["a"]});
    assert_eq!(
        h.post("/v1/rerank", &body).send().await.unwrap().status(),
        200
    );
    h.state.flush().await;
    let tokens: i64 = sqlx::query_scalar("SELECT input FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(tokens, 50);
    assert_eq!(
        h.post("/v1/rerank", &body).send().await.unwrap().status(),
        429
    );
    sqlx::query("UPDATE virtual_keys SET rpm_limit=NULL,allowed_models='[\"other\"]'")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(
        h.post("/v1/rerank", &body).send().await.unwrap().status(),
        403
    );
    sqlx::query("UPDATE virtual_keys SET allowed_models='[]',allowed_providers='[\"other\"]'")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(
        h.post("/v1/rerank", &body).send().await.unwrap().status(),
        403
    );
    let oversized = json!({"model":"route","query":"x".repeat(1024*1024),"documents":["a"]});
    assert_eq!(
        h.post("/v1/rerank", &oversized)
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
}

#[tokio::test]
async fn rerank_reserves_service_unit_budget_before_upstream() {
    let h = Harness::new(
        "cohere",
        r#"
budget_usd=0.01
[[models]]
name="upstream-model"
provider="upstream"
cost={service_units=1000000.0}
"#,
        axum::Router::new(),
    )
    .await;
    let response = h
        .post(
            "/v1/rerank",
            &json!({"model":"route","query":"q","documents":["a"]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 402);
}

#[tokio::test]
async fn count_tokens_forwards_without_charging_or_recording_tokens() {
    let h = Harness::new(
        "anthropic",
        "",
        axum::Router::new().route("/v1/messages/count_tokens", post(count_mock)),
    )
    .await;
    let response = h
        .post(
            "/v1/messages/count_tokens",
            &json!({"model":"route","messages":[{"role":"user","content":"hello"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("x-tokomotive-estimate"));
    assert_eq!(response.json::<Value>().await.unwrap()["input_tokens"], 42);
    h.state.flush().await;
    let entries: Vec<(String, i64, i64, i64)> =
        sqlx::query_as("SELECT endpoint,input,cost_micros,price_micros FROM ledger")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(entries, vec![("count_tokens".into(), 0, 0, 0)]);
    let estimated: i64 = sqlx::query_scalar("SELECT estimated FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(estimated, 0);
}

#[tokio::test]
async fn count_tokens_estimate_checks_permissions_size_and_request_limits() {
    let h = Harness::new(
        "openai",
        "key_rpm_limit=2\nkey_tpm_limit=1",
        axum::Router::new(),
    )
    .await;
    let body = json!({"model":"route","messages":[{"role":"user","content":"hello"}]});
    let response = h
        .post("/v1/messages/count_tokens", &body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-tokomotive-estimate"], "true");
    assert!(
        response.json::<Value>().await.unwrap()["input_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    h.state.flush().await;
    // The count does not consume TPM, but each call consumes one RPM.
    assert_eq!(
        h.post("/v1/messages/count_tokens", &body)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.post("/v1/messages/count_tokens", &body)
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    let oversized =
        json!({"model":"route","messages":[{"role":"user","content":"x".repeat(1024*1024)}]});
    let response = h
        .post("/v1/messages/count_tokens", &oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(response.json::<Value>().await.unwrap()["type"], "error");
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"other\"]'")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(
        h.post("/v1/messages/count_tokens", &body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    sqlx::query("UPDATE virtual_keys SET rpm_limit=NULL,tpm_limit=NULL,allowed_models='[]',allowed_providers='[\"other\"]'").execute(&h.state.db).await.unwrap();
    assert_eq!(
        h.post("/v1/messages/count_tokens", &body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}
