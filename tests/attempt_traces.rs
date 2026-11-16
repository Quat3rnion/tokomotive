use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::json;
use tokomotive::{config::Config, state::AppState};

struct Env {
    st: AppState,
    url: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn env() -> Env {
    let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", up.local_addr().unwrap());
    let app = axum::Router::new().route("/v1/chat/completions", post(|h: HeaderMap, Json(body):Json<serde_json::Value>| async move {
        if h["authorization"] == "Bearer sk-bad-1111" { return StatusCode::UNAUTHORIZED.into_response(); }
        if body["stream"]==true { return ([("content-type","text/event-stream")],"data: {\"choices\":[{\"delta\":{\"content\":\"private stream sk-streamsecret123\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n").into_response(); }
        Json(json!({"model":"m","choices":[{"message":{"content":"private response"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}})).into_response()
    }));
    let task = tokio::spawn(async move { axum::serve(up, app).await.unwrap() });
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database='sqlite::memory:'
mode='simple'
admin_password='password123'
[[providers]]
name='pool'
kind='openai'
base_url='{base}'
api_keys=['sk-bad-1111','sk-good-2222']
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
    Env {
        st,
        url,
        tasks: vec![task, proxy],
    }
}
impl Env {
    async fn send(&self) {
        let r = reqwest::Client::new().post(format!("{}/v1/chat/completions",self.url)).bearer_auth("sk-alice")
            .json(&json!({"model":"pool/m","messages":[{"role":"user","content":"private request sk-secret-123456789 Bearer abcdefghijklmnop AKIA1234567890123456"}]})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
        self.st.flush().await;
    }
}
#[tokio::test]
async fn failover_records_each_attempt_in_order_without_secrets() {
    let e = env().await;
    e.send().await;
    type AttemptSummary = (i64, String, Option<i64>, String, Option<String>);
    let rows: Vec<AttemptSummary> = sqlx::query_as("SELECT attempt_index,provider,status,error_class,key_last4 FROM request_attempts ORDER BY attempt_index").fetch_all(&e.st.db).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (
                1,
                "pool".into(),
                Some(401),
                "credentials_invalid".into(),
                Some("1111".into())
            ),
            (2, "pool".into(), Some(200), "".into(), Some("2222".into()))
        ]
    );
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM request_attempts a JOIN ledger l ON l.id=a.ledger_id WHERE l.attempts=2").fetch_one(&e.st.db).await.unwrap();
    assert_eq!(linked, 2);
}

#[tokio::test]
async fn skipped_credential_and_transport_failures_have_distinct_timeline_records() {
    let mut e = env().await;
    tokomotive::quota::record(
        &e.st.db,
        "pool",
        &tokomotive::quota::key_id("sk-bad-1111"),
        401,
        &HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    e.send().await;
    let a = tokomotive::observability::attempts(&e.st.db, 1)
        .await
        .unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].skip_reason, "credentials_invalid");
    assert_eq!(a[0].status, None);
    assert_eq!(a[1].status, Some(200));
    let task = e.tasks.remove(0);
    task.abort();
    let _ = task.await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", e.url))
        .bearer_auth("sk-alice")
        .json(&json!({"model":"pool/m","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    r.bytes().await.unwrap();
    e.st.flush().await;
    let a = tokomotive::observability::attempts(&e.st.db, 2)
        .await
        .unwrap();
    assert!(
        a.iter().any(|a| a.skip_reason.is_empty()
            && a.error_class == "transient"
            && a.status.is_none())
    );
}

#[tokio::test]
async fn request_log_filters_include_failed_attempts_and_retries() {
    let e = env().await;
    e.send().await;
    let q = tokomotive::db::LedgerQuery {
        status: Some(200),
        error_class: Some("credentials_invalid".into()),
        provider_key: Some(0),
        has_retries: Some(true),
        ..Default::default()
    };
    assert_eq!(
        tokomotive::db::ledger_page(&e.st.db, &q)
            .await
            .unwrap()
            .len(),
        1
    );
    let q = tokomotive::db::LedgerQuery {
        status: Some(401),
        ..q
    };
    assert!(
        tokomotive::db::ledger_page(&e.st.db, &q)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn body_capture_is_opt_in_redacted_capped_encrypted_and_purged() {
    let e = env().await;
    e.send().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM captured_bodies")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    sqlx::query("UPDATE virtual_keys SET capture_bodies=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    e.send().await;
    let (id, request, response): (i64, String, String) =
        sqlx::query_as("SELECT ledger_id,request,response FROM captured_bodies")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert!(!request.contains("private request"));
    assert!(!response.contains("private response"));
    let b = tokomotive::observability::bodies(&e.st.db, &e.st.secret_key, id)
        .await
        .unwrap()
        .unwrap();
    assert!(b.request.contains("private request"));
    for secret in ["sk-secret", "abcdefghijklmnop", "AKIA1234567890123456"] {
        assert!(!b.request.contains(secret));
    }
    assert!(b.response.contains("private response"));
    let mut s = (*e.st.observability.settings()).clone();
    s.body_cap = 10;
    e.st.observability.configure(s);
    e.send().await;
    let id: i64 = sqlx::query_scalar("SELECT MAX(ledger_id) FROM captured_bodies")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let b = tokomotive::observability::bodies(&e.st.db, &e.st.secret_key, id)
        .await
        .unwrap()
        .unwrap();
    assert!(b.request.len() <= 10 && b.response.len() <= 10);
    assert!(b.request_truncated && b.response_truncated);
    assert_eq!(
        tokomotive::observability::purge(&e.st.db, 7, tokomotive::db::now_ms() + 8 * 86400000)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn otlp_collector_receives_parent_and_attempt_spans_with_safe_attributes() {
    let e = env().await;
    sqlx::query("UPDATE virtual_keys SET label='client key'")
        .execute(&e.st.db)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let app = axum::Router::new().route(
        "/v1/traces",
        post(move |h: HeaderMap, Json(v): Json<serde_json::Value>| {
            let tx = tx.clone();
            async move {
                assert_eq!(h["x-collector-key"], "secret");
                tx.send(v).await.unwrap();
                StatusCode::OK
            }
        }),
    );
    let collector = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = tokomotive::observability::Settings {
        enabled: true,
        endpoint,
        headers: "x-collector-key: secret".into(),
        service_name: "router-test".into(),
        ..Default::default()
    };
    tokomotive::observability::save(&e.st.db, &e.st.secret_key, &s)
        .await
        .unwrap();
    let raw = tokomotive::settings::value(&e.st.db, "observability")
        .await
        .unwrap()
        .unwrap();
    assert!(!raw.contains("secret"));
    e.st.observability.configure(s);
    e.send().await;
    let v = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let resource = &v["resourceSpans"][0];
    assert_eq!(
        resource["resource"]["attributes"][0]["value"]["stringValue"],
        "router-test"
    );
    let spans = resource["scopeSpans"][0]["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 3);
    let attrs = spans[0]["attributes"].as_array().unwrap();
    assert!(
        attrs
            .iter()
            .any(|a| a["key"] == "gen_ai.system" && a["value"]["stringValue"] == "openai")
    );
    assert!(
        attrs
            .iter()
            .any(|a| a["key"] == "gen_ai.usage.input_tokens" && a["value"]["intValue"] == "3")
    );
    assert!(attrs.iter().any(|a|a["key"]=="tokomotive.key.name" && a["value"]["stringValue"]=="client key"));
    assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
    assert_eq!(spans[2]["traceId"], spans[0]["traceId"]);
    assert!(!v.to_string().contains("sk-bad"));
    collector.abort();
}

#[test]
fn body_redaction_removes_header_fields_and_bearer_values() {
    let raw = r#"{"Authorization":"Bearer private-token","x-api-key":"private-key","access_token":"oauth-token","note":"Bearer opaque-token sk-123456789 AKIA1234567890123456"}"#;
    let redacted = tokomotive::observability::redact(raw);
    for secret in [
        "private-token",
        "private-key",
        "oauth-token",
        "opaque-token",
        "sk-123456789",
        "AKIA1234567890123456",
    ] {
        assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
    }
}

#[tokio::test]
async fn bounded_export_queue_drops_without_waiting_for_a_collector() {
    let e = env().await;
    e.st.observability
        .configure(tokomotive::observability::Settings {
            enabled: true,
            endpoint: "http://127.0.0.1:9/v1/traces".into(),
            ..Default::default()
        });
    let t = tokomotive::observability::RequestTrace::new(&e.st, None, 1, b"", 8).await;
    let entry = tokomotive::db::NewEntry {
        route_rule: None,
        ts_ms: tokomotive::db::now_ms(),
        user_id: 1,
        key_id: 1,
        team_id: None,
        provider: "pool".into(),
        subscription: false,
        endpoint: "chat_completions".into(),
        requested_model: Some("m".into()),
        model: Some("m".into()),
        status: 200,
        streamed: false,
        cache_hit: false,
        usage: Default::default(),
        reported_cost_micros: None,
        quote: Default::default(),
        estimated: false,
        latency_ms: 1,
        ttft_ms: None,
        client_aborted: false,
        provider_key: None,
        attempts: 1,
        session_affinity: None,
    };
    let start = std::time::Instant::now();
    for _ in 0..300 {
        e.st.observability.export(&entry, &t, "alice", "client key");
    }
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(e.st.observability.drops(), 172);
}

#[tokio::test]
async fn route_capture_records_stream_events_and_attempt_ttft() {
    let e = env().await;
    sqlx::query("INSERT INTO routes(name,config_json,capture_bodies) VALUES('chat',?,1)")
        .bind(json!({"name":"chat","deployments":[{"provider":"pool","model":"m"}]}).to_string())
        .execute(&e.st.db)
        .await
        .unwrap();
    e.st.reload_settings().await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", e.url))
        .bearer_auth("sk-alice")
        .json(&json!({"model":"chat","messages":[{"role":"user","content":"hi"}],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    e.st.flush().await;
    let id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM ledger")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let a = tokomotive::observability::attempts(&e.st.db, id)
        .await
        .unwrap();
    assert_eq!(a.len(), 2);
    assert!(a[1].ttft_ms.is_some());
    let b = tokomotive::observability::bodies(&e.st.db, &e.st.secret_key, id)
        .await
        .unwrap()
        .unwrap();
    assert!(b.response.contains("private stream"));
    assert!(!b.response.contains("sk-streamsecret"));
}

#[tokio::test]
async fn admin_can_view_timeline_and_bodies_but_virtual_key_cannot() {
    let e = env().await;
    sqlx::query("UPDATE virtual_keys SET capture_bodies=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    e.send().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let path = format!("{}/admin/ledger?entry=1", e.url);
    let r = client
        .get(&path)
        .bearer_auth("sk-alice")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert!(!r.text().await.unwrap().contains("private request"));
    let r = client
        .post(format!("{}/admin/login", e.url))
        .form(&[("password", "password123")])
        .send()
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|h| {
            h.to_str()
                .ok()
                .filter(|v| v.starts_with("tokomotive_session="))
        })
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let page = client
        .get(&path)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("attempt timeline"));
    assert!(page.contains("credentials_invalid"));
    assert!(page.contains("Key 1"));
    assert!(page.contains("private request"));
    assert!(page.contains("private response"));
    assert!(!page.contains("sk-secret"));
    let page = client
        .get(format!("{}/admin/settings", e.url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Save observability settings"));
    let r = client
        .post(format!("{}/admin/settings/observability", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("endpoint", "http://127.0.0.1:9/v1/traces"),
            ("headers", "Authorization: privatecollector"),
            ("service_name", "test"),
            ("sample_ratio", "0.5"),
            ("body_cap", "65536"),
            ("retention_days", "7"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let page = client
        .get(format!("{}/admin/settings", e.url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("********"));
    assert!(!page.contains("privatecollector"));
    let metrics = client
        .get(format!("{}/metrics", e.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("tokomotive_trace_export_queue_drops_total"));
    assert!(metrics.contains("tokomotive_captured_bodies_total"));
    sqlx::query("INSERT INTO routes(name,config_json,capture_bodies) VALUES('chat',?,1)")
        .bind(json!({"name":"chat","deployments":[{"provider":"pool","model":"m"}]}).to_string())
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = client
        .post(format!("{}/admin/routes/chat", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("name", "renamed"),
            ("strategy", "priority"),
            ("deployment_provider", "pool"),
            ("deployment_model", "m"),
            ("deployment_weight", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let enabled: bool =
        sqlx::query_scalar("SELECT capture_bodies FROM routes WHERE name='renamed'")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert!(enabled, "Route edits must preserve body capture");
    let r = client
        .get(format!(
            "{}/admin/ledger?provider_key=-9223372036854775808",
            e.url
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// Captured prompts and responses are sensitive. A read-only Viewer can open
/// the Ledger Entry, but the server must not decrypt or render its bodies.
#[tokio::test]
async fn viewer_sees_the_timeline_but_never_captured_bodies() {
    let e = env().await;
    sqlx::query("UPDATE virtual_keys SET capture_bodies=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    e.send().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let session = |r: reqwest::Response| {
        r.headers()
            .get_all("set-cookie")
            .iter()
            .find_map(|h| {
                h.to_str()
                    .ok()
                    .filter(|v| v.starts_with("tokomotive_session="))
            })
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let owner = session(
        client
            .post(format!("{}/admin/login", e.url))
            .form(&[("password", "password123")])
            .send()
            .await
            .unwrap(),
    );
    let r = client
        .post(format!("{}/admin/settings/accounts", e.url))
        .header("cookie", &owner)
        .header("origin", &e.url)
        .form(&[
            ("username", "reader"),
            ("role", "viewer"),
            ("password", "account-password"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let viewer = session(
        client
            .post(format!("{}/admin/login", e.url))
            .form(&[("username", "reader"), ("password", "account-password")])
            .send()
            .await
            .unwrap(),
    );
    let page = client
        .get(format!("{}/admin/ledger?entry=1", e.url))
        .header("cookie", &viewer)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("attempt timeline"));
    assert!(
        !page.contains("private request"),
        "Viewer saw a captured request"
    );
    assert!(
        !page.contains("private response"),
        "Viewer saw a captured response"
    );
    assert!(!page.contains("Captured bodies"));
}
