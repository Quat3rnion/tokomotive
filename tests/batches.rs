use axum::{
    Json,
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};

struct Harness {
    _dir: tempfile::TempDir,
    state: tokomotive::state::AppState,
    url: String,
    client: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Harness {
    async fn new(upstream: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let cfg = toml::from_str(&format!(
            r#"
[server]
database="sqlite://{}/db.sqlite"
[[providers]]
name="upstream"
kind="openai"
base_url="http://{address}"
api_keys=["private-key", "other-key"]
[[users]]
name="alice"
keys=["sk-alice"]
budget_usd=10
[[users]]
name="bob"
keys=["sk-bob"]
[[models]]
name="gpt-test"
provider="upstream"
cost={{input=1.0,output=2.0,batch_multiplier_bp=5000}}
"#,
            dir.path().display()
        ))
        .unwrap();
        static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
            std::sync::OnceLock::new();
        let state = tokomotive::init(
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
        let app = tokomotive::router(state.clone());
        let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            _dir: dir,
            state,
            url,
            client: reqwest::Client::new(),
            tasks: vec![upstream, proxy],
        }
    }
    fn request(&self, method: reqwest::Method, path: &str, key: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(key)
    }
    fn upload(&self, content: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, "/v1/files", "sk-alice")
            .header("x-tokomotive-provider", "upstream")
            .multipart(
                reqwest::multipart::Form::new()
                    .text("purpose", "batch")
                    .part(
                        "file",
                        reqwest::multipart::Part::text(content.to_owned()).file_name("input.jsonl"),
                    ),
            )
    }
}

const INPUT: &str = "{\"custom_id\":\"one\",\"method\":\"POST\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"gpt-test\",\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}],\"max_tokens\":10}}\n";

#[tokio::test]
async fn files_are_pinned_to_one_provider_key_and_private_to_the_virtual_key() {
    async fn upload(headers: HeaderMap, body: axum::body::Bytes) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer private-key");
        assert!(String::from_utf8_lossy(&body).contains("gpt-test"));
        Json(
            json!({"id":"file-input","object":"file","purpose":"batch","bytes":200,"created_at":1}),
        )
    }
    async fn file(headers: HeaderMap) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer private-key");
        Json(json!({"id":"file-input","object":"file","purpose":"batch"}))
    }
    let h = Harness::new(
        axum::Router::new()
            .route("/v1/files", post(upload))
            .route(
                "/v1/files/file-input",
                get(file).delete(|| async {
                    Json(json!({"id":"file-input","object":"file","deleted":true}))
                }),
            )
            .route("/v1/files/file-input/content", get(|| async { INPUT })),
    )
    .await;
    let response = h.upload(INPUT).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["id"], "file-input");
    let mut table = (*h.state.router_table()).clone();
    table.providers[0].keys = std::sync::Arc::new(vec!["other-key".into(), "private-key".into()]);
    h.state.set_router_table(table);
    // Sharing a User is not sufficient to share an upstream object.
    sqlx::query("UPDATE virtual_keys SET user_id=(SELECT id FROM users WHERE name='alice') WHERE key_hash=?").bind(tokomotive::db::hash_key("sk-bob")).execute(&h.state.db).await.unwrap();
    for path in ["/v1/files/file-input", "/v1/files/file-input/content"] {
        assert_eq!(
            h.request(reqwest::Method::GET, path, "sk-bob")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            h.request(reqwest::Method::GET, path, "sk-alice")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        h.request(reqwest::Method::DELETE, "/v1/files/file-input", "sk-bob")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for (key, n) in [("sk-alice", 1), ("sk-bob", 0)] {
        let list = h
            .request(reqwest::Method::GET, "/v1/files?limit=1", key)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(list["data"].as_array().unwrap().len(), n);
    }
    assert_eq!(
        h.request(reqwest::Method::DELETE, "/v1/files/file-input", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/files/file-input", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

fn batch_upstream() -> axum::Router {
    batch_upstream_with_output(get(|| async {
        "{\"custom_id\":\"one\",\"response\":{\"status_code\":200,\"body\":{\"model\":\"gpt-test\",\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50}}}}\n"
    }))
}
fn batch_upstream_with_output(output: axum::routing::MethodRouter) -> axum::Router {
    batch_upstream_with_result(
        json!({"id":"batch-one","object":"batch","status":"completed","output_file_id":"file-output","input_file_id":"file-input"}),
        output,
    )
}
fn batch_upstream_with_result(result: Value, output: axum::routing::MethodRouter) -> axum::Router {
    axum::Router::new()
        .route("/v1/files",post(|| async {Json(json!({"id":"file-input","object":"file","purpose":"batch"}))}))
        .route("/v1/batches",post(|headers:HeaderMap,Json(body):Json<Value>|async move {
            assert_eq!(headers["authorization"],"Bearer private-key");
            assert_eq!(body["input_file_id"],"file-input");
            Json(json!({"id":"batch-one","object":"batch","status":"in_progress","input_file_id":"file-input"}))
        }))
        .route("/v1/batches/batch-one",get(move ||{let result=result.clone();async move {Json(result)}}))
        .route("/v1/batches/batch-one/cancel",post(||async {Json(json!({"id":"batch-one","object":"batch","status":"cancelling","input_file_id":"file-input"}))}))
        .route("/v1/files/file-output/content",output)
        .route("/v1/files/file-output",get(|headers:HeaderMap|async move {
            assert_eq!(headers["authorization"],"Bearer private-key");
            Json(json!({"id":"file-output","object":"file","purpose":"batch_output","bytes":147,"filename":"result.jsonl","created_at":1}))
        }))
}

#[tokio::test]
async fn contended_batch_keeps_durable_job_and_reservation_then_accounts_once() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let first = Arc::new(AtomicBool::new(true));
    let output = get({
        let entered = entered.clone();
        let release = release.clone();
        move || {
            let entered = entered.clone();
            let release = release.clone();
            let first = first.clone();
            async move {
                if first.swap(false, Ordering::SeqCst) {
                    entered.notify_one();
                    release.notified().await;
                }
                "{\"custom_id\":\"one\",\"response\":{\"status_code\":200,\"body\":{\"model\":\"gpt-test\",\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50}}}}\n"
            }
        }
    });
    let h = Harness::new(batch_upstream_with_output(output)).await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST, "/v1/batches", "sk-alice")
        .json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"}))
        .send().await.unwrap().status(), 200);
    sqlx::query("UPDATE settings SET value=json_set(value,'$.writer_timeout_secs',1) WHERE key='runtime_settings'")
        .execute(&h.state.db).await.unwrap();
    h.state.reload_settings().await.unwrap();
    let before: (i64, i64, String) = sqlx::query_as(
        "SELECT accounted,held_micros,snapshot_json FROM batch_jobs WHERE id='batch-one'",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!(before.0, 0);
    assert!(before.1 > 0);
    let state = h.state.clone();
    let polling = tokio::spawn(async move { tokomotive::batches::poll_once(&state).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    let writer = h.state.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), polling)
        .await
        .expect("batch writer acquisition is bounded")
        .unwrap()
        .unwrap();
    let after: (i64, i64, String) = sqlx::query_as(
        "SELECT accounted,held_micros,snapshot_json FROM batch_jobs WHERE id='batch-one'",
    )
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "a failed BEGIN must retain the durable job and its full Reservation"
    );
    writer.rollback().await.unwrap();
    tokomotive::batches::poll_once(&h.state).await.unwrap();
    tokomotive::batches::poll_once(&h.state).await.unwrap();
    let job: (i64, i64) =
        sqlx::query_as("SELECT accounted,held_micros FROM batch_jobs WHERE id='batch-one'")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(job, (1, 0));
    let totals: (i64, i64, i64) =
        sqlx::query_as("SELECT COUNT(*),SUM(input),SUM(output) FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(totals, (1, 100, 50));
    let requests: i64 = sqlx::query_scalar("SELECT SUM(requests) FROM rollup_hourly")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn batch_completion_accounts_once_with_frozen_discount_and_owned_output() {
    let h = Harness::new(batch_upstream()).await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    let create = || {
        h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"}))
    };
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-bob").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),404);
    assert_eq!(create().send().await.unwrap().status(), 200);
    for method_path in [
        (reqwest::Method::GET, "/v1/batches/batch-one"),
        (reqwest::Method::POST, "/v1/batches/batch-one/cancel"),
    ] {
        assert_eq!(
            h.request(method_path.0, method_path.1, "sk-bob")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    assert_eq!(
        h.request(
            reqwest::Method::POST,
            "/v1/batches/batch-one/cancel",
            "sk-alice"
        )
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    // Card edits after create must not change the frozen rates.
    sqlx::query("INSERT INTO cards(model_id,team_id,kind,spec,created_at) SELECT model_id,team_id,kind,json_set(spec,'$.base.input',9000000),created_at+1 FROM cards WHERE kind='cost'").execute(&h.state.db).await.unwrap();
    h.state.reload_catalog().await.unwrap();
    for _ in 0..2 {
        let response = h
            .request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    h.state.flush().await;
    let rows: Vec<(String, i64, i64, i64, i64)> =
        sqlx::query_as("SELECT endpoint,input,output,cost_micros,price_micros FROM ledger")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(rows, vec![("batch".into(), 100, 50, 100, 100)]);
    assert_eq!(
        h.request(
            reqwest::Method::GET,
            "/v1/files/file-output/content",
            "sk-bob"
        )
        .send()
        .await
        .unwrap()
        .status(),
        404
    );
    assert_eq!(
        h.request(
            reqwest::Method::GET,
            "/v1/files/file-output/content",
            "sk-alice"
        )
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    let batches = h
        .request(reqwest::Method::GET, "/v1/batches", "sk-alice")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(batches["data"][0]["status"], "completed");
    let files = h
        .request(
            reqwest::Method::GET,
            "/v1/files?purpose=batch_output",
            "sk-alice",
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(files["data"][0]["bytes"], 147);
    assert_eq!(files["data"][0]["filename"], "result.jsonl");
}

#[tokio::test]
async fn poller_accounts_without_a_client_get_and_is_idempotent() {
    let h = Harness::new(batch_upstream()).await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    for _ in 0..2 {
        tokomotive::batches::poll_once(&h.state).await.unwrap();
    }
    h.state.flush().await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let held: i64 = sqlx::query_scalar("SELECT held_micros FROM batch_jobs")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(held, 0);
}

#[tokio::test]
async fn file_upload_limits_model_permissions_and_rate_limits_apply() {
    let h = Harness::new(batch_upstream()).await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.file_upload_limit_mib',1) WHERE key='runtime_settings'").execute(&h.state.db).await.unwrap();
    h.state.reload_settings().await.unwrap();
    assert_eq!(
        h.upload(&"x".repeat(1024 * 1024))
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"other\"]' WHERE key_hash=?")
        .bind(tokomotive::db::hash_key("sk-alice"))
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 403);
    sqlx::query("UPDATE virtual_keys SET allowed_models='[]',rpm_limit=0 WHERE key_hash=?")
        .bind(tokomotive::db::hash_key("sk-alice"))
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 429);
}

#[tokio::test]
async fn batch_reservations_survive_reload_and_input_permissions_are_rechecked() {
    let h = Harness::new(batch_upstream()).await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    let create = || {
        h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"}))
    };
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"other\"]' WHERE key_hash=?")
        .bind(tokomotive::db::hash_key("sk-alice"))
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(create().send().await.unwrap().status(), 403);
    sqlx::query("UPDATE virtual_keys SET allowed_models='[]' WHERE key_hash=?")
        .bind(tokomotive::db::hash_key("sk-alice"))
        .execute(&h.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET budget_micros=NULL")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(create().send().await.unwrap().status(), 400);
    sqlx::query("UPDATE users SET budget_micros=1 WHERE name='alice'")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(create().send().await.unwrap().status(), 402);
    sqlx::query("UPDATE users SET budget_micros=1000000 WHERE name='alice'")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(create().send().await.unwrap().status(), 200);
    let held: i64 = sqlx::query_scalar("SELECT held_micros FROM batch_jobs")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(held > 0);
    sqlx::query("UPDATE users SET budget_micros=? WHERE name='alice'")
        .bind(held)
        .execute(&h.state.db)
        .await
        .unwrap();
    h.state.reload_settings().await.unwrap();
    assert_eq!(create().send().await.unwrap().status(), 402);
    let auth = tokomotive::db::lookup_key(&h.state.db, "sk-alice")
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokomotive::budgets::reserve(&h.state, &auth, 1, tokomotive::db::now_s())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn simple_mode_uses_no_money_and_absent_batch_multiplier_uses_normal_rates() {
    for simple in [false, true] {
        let h = Harness::new(batch_upstream()).await;
        sqlx::query("INSERT INTO cards(model_id,kind,spec,created_at) SELECT model_id,kind,json_remove(spec,'$.batch_multiplier_bp'),created_at+1 FROM cards WHERE kind='cost'").execute(&h.state.db).await.unwrap();
        h.state.reload_catalog().await.unwrap();
        if simple {
            sqlx::query("UPDATE settings SET value=json_set(value,'$.mode','simple') WHERE key='runtime_settings'").execute(&h.state.db).await.unwrap();
            sqlx::query("UPDATE users SET budget_micros=0,rpm_limit=0")
                .execute(&h.state.db)
                .await
                .unwrap();
            h.state.reload_settings().await.unwrap();
        }
        assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
        assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
        assert_eq!(
            h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let amounts: (i64, i64) = sqlx::query_as("SELECT cost_micros,price_micros FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
        assert_eq!(amounts, if simple { (0, 0) } else { (200, 200) });
    }
}

#[tokio::test]
async fn discounted_batch_amount_rounds_only_once() {
    let h=Harness::new(batch_upstream_with_output(get(||async {"{\"custom_id\":\"one\",\"response\":{\"status_code\":200,\"body\":{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":0}}}}\n"}))).await;
    sqlx::query("INSERT INTO cards(model_id,kind,spec,created_at) SELECT model_id,kind,json_set(spec,'$.base.input',750000),created_at+1 FROM cards WHERE kind='cost'").execute(&h.state.db).await.unwrap();
    h.state.reload_catalog().await.unwrap();
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let cost: i64 = sqlx::query_scalar("SELECT cost_micros FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    // 1 token * $0.75/Mtok * 50% = 0.375 micro-dollars, which rounds to zero.
    assert_eq!(cost, 0);
}

#[tokio::test]
async fn unaccounted_batch_output_cannot_be_deleted_and_missing_output_is_not_free() {
    let h = Harness::new(
        batch_upstream_with_output(get(|| async {
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }))
        .route(
            "/v1/files/file-output",
            axum::routing::delete(|| async { Json(json!({"id":"file-output","deleted":true})) }),
        ),
    )
    .await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        502
    );
    assert_eq!(
        h.request(reqwest::Method::DELETE, "/v1/files/file-output", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let held: i64 = sqlx::query_scalar("SELECT held_micros FROM batch_jobs WHERE accounted=0")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(held > 0);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn completed_batch_without_output_does_not_release_its_reservation() {
    let h = Harness::new(batch_upstream_with_result(
        json!({"id":"batch-one","object":"batch","status":"completed"}),
        get(|| async { "" }),
    ))
    .await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        502
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn batch_output_limits_must_match_the_endpoint_and_paid_tools_are_rejected() {
    let h = Harness::new(batch_upstream()).await;
    let mut line: Value = serde_json::from_str(INPUT).unwrap();
    line["url"] = json!("/v1/responses");
    assert_eq!(
        h.upload(&line.to_string()).send().await.unwrap().status(),
        400
    );
    line["url"] = json!("/v1/chat/completions");
    line["body"]["tools"] = json!([{"type":"web_search_2026"}]);
    assert_eq!(
        h.upload(&line.to_string()).send().await.unwrap().status(),
        400
    );
}

#[tokio::test]
async fn batch_rejects_embedded_documents_and_nested_background_requests() {
    let h = Harness::new(batch_upstream()).await;
    let mut line: Value = serde_json::from_str(INPUT).unwrap();
    line["body"]["messages"][0]["content"] = json!([{"type":"file","file":{"filename":"input.pdf","file_data":"data:application/pdf;base64,JVBERi0="}}]);
    assert_eq!(
        h.upload(&line.to_string()).send().await.unwrap().status(),
        400
    );
    line = json!({"custom_id":"one","method":"POST","url":"/v1/responses","body":{"model":"gpt-test","input":"hello","max_output_tokens":10,"background":true}});
    assert_eq!(
        h.upload(&line.to_string()).send().await.unwrap().status(),
        400
    );
}

#[tokio::test]
async fn files_and_batches_use_the_existing_proxy_error_types() {
    let h = Harness::new(batch_upstream()).await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    sqlx::query("UPDATE users SET budget_micros=1 WHERE name='alice'")
        .execute(&h.state.db)
        .await
        .unwrap();
    let response=h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap();
    assert_eq!(response.status(), 402);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "budget_exceeded"
    );
}

#[tokio::test]
async fn local_lists_use_owned_cursors_and_file_content_streams() {
    use axum::response::IntoResponse;
    use futures_util::StreamExt;
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let upstream_release = release.clone();
    let h = Harness::new(
        axum::Router::new()
            .route(
                "/v1/files",
                post(move || {
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    async move {
                        Json(json!({"id":format!("file-{n}"),"object":"file","purpose":"batch"}))
                    }
                }),
            )
            .route(
                "/v1/files/file-1/content",
                get(move || {
                    let release = upstream_release.clone();
                    async move {
                        let chunks =
                            futures_util::stream::once(async { Ok::<_, std::io::Error>("first") })
                                .chain(futures_util::stream::once(async move {
                                    release.notified().await;
                                    Ok("last")
                                }));
                        (
                            [("content-type", "application/octet-stream")],
                            axum::body::Body::from_stream(chunks),
                        )
                            .into_response()
                    }
                }),
            ),
    )
    .await;
    for _ in 0..3 {
        assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    }
    let first = h
        .request(reqwest::Method::GET, "/v1/files?limit=2", "sk-alice")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(first["has_more"], true);
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let after = first["last_id"].as_str().unwrap();
    let second = h
        .request(
            reqwest::Method::GET,
            &format!("/v1/files?limit=2&after={after}"),
            "sk-alice",
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(second["has_more"], false);
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/files?after=file-1", "sk-bob")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let mut response = h
        .request(reqwest::Method::GET, "/v1/files/file-1/content", "sk-alice")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.chunk().await.unwrap().unwrap(), "first");
    release.notify_one();
    assert_eq!(response.bytes().await.unwrap(), "last");
}

#[tokio::test]
async fn poller_has_a_twenty_batch_work_limit() {
    let h = Harness::new(batch_upstream().route(
        "/v1/batches/{id}",
        get(
            |axum::extract::Path(id): axum::extract::Path<String>| async move {
                Json(json!({"id":id,"object":"batch","status":"in_progress"}))
            },
        ),
    ))
    .await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    for n in 0..25 {
        let id = format!("batch-clone-{n:02}");
        sqlx::query("INSERT INTO proxy_objects SELECT kind,?,key_id,provider,provider_key,credential_hash,base_url,created_at,status,json_set(object_json,'$.id',?),manifest_json FROM proxy_objects WHERE kind='batch' AND id='batch-one'").bind(&id).bind(&id).execute(&h.state.db).await.unwrap();
        sqlx::query("INSERT INTO batch_jobs SELECT ?,key_id,user_id,team_id,held_micros,snapshot_json,simple,created_at,0,status,accounted FROM batch_jobs WHERE id='batch-one'").bind(id).execute(&h.state.db).await.unwrap();
    }
    assert_eq!(tokomotive::batches::poll_once(&h.state).await.unwrap(), 20);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM batch_jobs WHERE id!='batch-one' AND last_poll>0")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(n, 20);
}

#[tokio::test]
async fn unsupported_owned_object_paths_never_use_the_generic_proxy() {
    let h = Harness::new(axum::Router::new().route(
        "/v1/{*rest}",
        axum::routing::any(|| async { Json(json!({"leaked":true})) }),
    ))
    .await;
    for path in [
        "/v1/files/some-file/extra",
        "/v1/files//some-file/content",
        "/v1/batches/some-batch/extra",
        "/v1/%66iles/some-file",
        "/v1/%62atches/some-batch",
        "/v1/files%2fsome-file%2fcontent",
    ] {
        assert_eq!(
            h.request(reqwest::Method::GET, path, "sk-alice")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
}

#[tokio::test]
async fn terminal_failed_expired_and_cancelled_batches_still_charge_completed_lines() {
    for status in ["failed", "expired", "cancelled"] {
        let h=Harness::new(batch_upstream_with_result(json!({"id":"batch-one","object":"batch","status":status,"output_file_id":"file-output","request_counts":{"completed":1}}),get(||async {"{\"custom_id\":\"one\",\"response\":{\"status_code\":200,\"body\":{\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50}}}}\n"}))).await;
        assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
        assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
        assert_eq!(
            h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT cost_micros FROM ledger")
                .fetch_one(&h.state.db)
                .await
                .unwrap(),
            100
        );
    }
}

#[tokio::test]
async fn oversized_jsonl_lines_are_rejected_before_upload_and_before_accounting() {
    let mut line: Value = serde_json::from_str(INPUT).unwrap();
    line["body"]["messages"][0]["content"] = json!("x".repeat(1024 * 1024));
    let mut output=json!({"custom_id":"one","padding":"x".repeat(1024*1024),"response":{"status_code":200,"body":{"usage":{"prompt_tokens":100,"completion_tokens":50}}}}).to_string();
    output.push('\n');
    let h = Harness::new(batch_upstream_with_output(get(move || {
        let output = output.clone();
        async move { output }
    })))
    .await;
    assert_eq!(
        h.upload(&line.to_string()).send().await.unwrap().status(),
        400
    );
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),200);
    assert_eq!(
        h.request(reqwest::Method::GET, "/v1/batches/batch-one", "sk-alice")
            .send()
            .await
            .unwrap()
            .status(),
        502
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger")
            .fetch_one(&h.state.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ambiguous_create_failure_keeps_a_durable_hold_and_is_not_retried() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream_calls = calls.clone();
    let h = Harness::new(
        axum::Router::new()
            .route(
                "/v1/files",
                post(|| async {
                    Json(json!({"id":"file-input","object":"file","purpose":"batch"}))
                }),
            )
            .route(
                "/v1/batches",
                post(move || {
                    upstream_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error":{"message":"uncertain"}})),
                        )
                    }
                }),
            ),
    )
    .await;
    assert_eq!(h.upload(INPUT).send().await.unwrap().status(), 200);
    assert_eq!(h.request(reqwest::Method::POST,"/v1/batches","sk-alice").json(&json!({"input_file_id":"file-input","endpoint":"/v1/chat/completions","completion_window":"24h"})).send().await.unwrap().status(),500);
    assert_eq!(tokomotive::batches::poll_once(&h.state).await.unwrap(), 0);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let row: (String, i64) = sqlx::query_as("SELECT status,held_micros FROM batch_jobs")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(row.0, "uncertain");
    assert!(row.1 > 0);
}
