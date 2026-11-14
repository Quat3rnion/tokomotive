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
        let state = tokomotive::init(&cfg, None).await.unwrap();
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
