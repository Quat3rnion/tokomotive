//! Real WAL contention: busy-handler expiry must not discard accounting writes.
use axum::{Json, http::HeaderMap, routing::post};
use serde_json::json;
use std::time::Duration;
use tokomotive::{config::Config, quota};

fn metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| tokomotive::metrics::install().unwrap())
        .clone()
}

async fn spool_state(dir: &std::path::Path, cap: u64) -> (Config, tokomotive::state::AppState) {
    let cfg: Config = toml::from_str(&format!(
        "[server]\ndatabase='sqlite://{}/db.sqlite'\n[[users]]\nname='alice'\nkeys=['key']",
        dir.display()
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, Some(metrics_handle()))
        .await
        .unwrap();
    sqlx::query("UPDATE settings SET value=json_set(value,'$.writer_timeout_secs',1,'$.ledger_spool_max_bytes',?) WHERE key='runtime_settings'")
        .bind(cap as i64).execute(&st.db).await.unwrap();
    st.reload_settings().await.unwrap();
    (cfg, st)
}

fn entry() -> tokomotive::db::NewEntry {
    tokomotive::db::NewEntry {
        cache_hit: false,
        ts_ms: tokomotive::db::now_ms(),
        user_id: 1,
        key_id: 1,
        team_id: None,
        provider: "test".into(),
        subscription: false,
        endpoint: "chat/completions".into(),
        requested_model: Some("model".into()),
        model: Some("model".into()),
        status: 200,
        streamed: false,
        usage: tokomotive::usage::Usage {
            input: 3,
            output: 7,
            ..Default::default()
        },
        reported_cost_micros: None,
        quote: tokomotive::catalog::Quote {
            cost_micros: 12,
            price_micros: 24,
            ..Default::default()
        },
        estimated: false,
        latency_ms: 20,
        ttft_ms: None,
        client_aborted: false,
        provider_key: None,
        attempts: 1,
        session_affinity: None,
    }
}

async fn assert_one_entry(db: &tokomotive::db::Db) {
    let totals: (i64, i64, i64) =
        sqlx::query_as("SELECT COUNT(*),SUM(input+output),SUM(price_micros) FROM ledger")
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(totals, (1, 10, 24));
    let requests: i64 = sqlx::query_scalar("SELECT SUM(requests) FROM rollup_hourly")
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(requests, 1, "replay must not duplicate the rollup either");
}

#[tokio::test]
async fn held_writer_spools_and_replays_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_, st) = spool_state(dir.path(), 65536).await;
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let id = tokio::time::timeout(
        Duration::from_secs(2),
        tokomotive::db::insert_entry(&st.db, &entry()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        id.is_none(),
        "a held writer must use durable storage within the hard limit"
    );
    let path = dir.path().join("ledger-spool.jsonl");
    let record = std::fs::read(&path).unwrap();
    assert_eq!(record.last(), Some(&b'\n'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    writer.rollback().await.unwrap();
    assert_eq!(tokomotive::db::replay_spool(&st.db).await.unwrap(), 1);
    assert_one_entry(&st.db).await;
    // Simulate a crash after the commit but before removal of the segment.
    std::fs::write(path.with_extension("replay.jsonl"), &record).unwrap();
    assert_eq!(tokomotive::db::replay_spool(&st.db).await.unwrap(), 1);
    assert_one_entry(&st.db).await;
    assert_eq!(tokomotive::db::replay_spool(&st.db).await.unwrap(), 0);
}

#[tokio::test]
async fn incomplete_spool_tail_cannot_hide_a_new_durable_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (_, st) = spool_state(dir.path(), 65536).await;
    let path = dir.path().join("ledger-spool.jsonl");
    std::fs::write(&path, b"{\"write_key\":\"partial").unwrap();
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let result = tokomotive::db::insert_entry(&st.db, &entry()).await;
    writer.rollback().await.unwrap();
    assert!(
        result.is_err(),
        "do not accept a record behind an incomplete JSONL tail"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"write_key\":\"partial");
}

#[tokio::test]
async fn restart_replays_a_durable_spooled_entry() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, st) = spool_state(dir.path(), 65536).await;
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        tokomotive::db::insert_entry(&st.db, &entry())
            .await
            .unwrap()
            .is_none()
    );
    writer.rollback().await.unwrap();
    st.db.close().await;
    drop(st);
    let restarted = tokomotive::init(&cfg, None).await.unwrap();
    assert_one_entry(&restarted.db).await;
    assert!(!dir.path().join("ledger-spool.replay.jsonl").exists());
}

#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn full_spool_is_logged_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let (_, st) = spool_state(dir.path(), 1).await;
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let error = tokomotive::db::insert_entry(&st.db, &entry())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("spool is full"));
    writer.rollback().await.unwrap();
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("Ledger spool is full"), "{text}");
    let metrics = metrics_handle().render();
    assert!(
        metrics.contains("tokomotive_ledger_spool_full_total 1"),
        "{metrics}"
    );
}

#[tokio::test]
async fn writer_warning_names_caller_and_wait_metrics_are_balanced() {
    let dir = tempfile::tempdir().unwrap();
    let (_, st) = spool_state(dir.path(), 65536).await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.writer_timeout_secs',11) WHERE key='runtime_settings'")
        .execute(&st.db).await.unwrap();
    st.reload_settings().await.unwrap();
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish(),
    );
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let waiting = quota::take_health_events(&st.db);
    tokio::pin!(waiting);
    tokio::select! {
        _ = &mut waiting => panic!("writer unexpectedly acquired"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {},
    }
    let metrics = metrics_handle().render();
    let waiting_count: f64 = metrics
        .lines()
        .find_map(|line| {
            line.strip_prefix("tokomotive_db_waiting_writers{caller=\"health_events\"} ")
                .and_then(|v| v.parse().ok())
        })
        .unwrap();
    assert!(waiting_count >= 1.0, "{metrics}");
    assert!(
        tokio::time::timeout(Duration::from_secs(12), waiting)
            .await
            .unwrap()
            .is_err()
    );
    writer.rollback().await.unwrap();
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("SQLite writer is still waiting") && text.contains("health_events"),
        "{text}"
    );
    let metrics = metrics_handle().render();
    for expected in [
        "tokomotive_db_waiting_writers{caller=\"health_events\"} 0",
        "tokomotive_db_writer_wait_seconds_count{caller=\"health_events\"}",
        "tokomotive_db_writer_warnings_total{caller=\"health_events\"}",
        "tokomotive_db_writer_timeouts_total{caller=\"health_events\"}",
    ] {
        assert!(metrics.contains(expected), "missing {expected}: {metrics}");
    }
}

async fn shorten_busy_timeout(db: &tokomotive::db::Db) {
    // Exercise busy-handler expiry quickly, on every connection in the pool.
    let mut connections = Vec::new();
    for _ in 0..db.options().get_max_connections() {
        let mut conn = db.acquire().await.unwrap();
        sqlx::query("PRAGMA busy_timeout=10")
            .execute(&mut *conn)
            .await
            .unwrap();
        connections.push(conn);
    }
}

#[tokio::test]
async fn held_writer_returns_at_configured_hard_limit() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&format!(
        "[server]\ndatabase='sqlite://{}/db.sqlite'",
        dir.path().display()
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    sqlx::query("UPDATE settings SET value=json_set(value,'$.writer_timeout_secs',1) WHERE key='runtime_settings'")
        .execute(&st.db).await.unwrap();
    st.reload_settings().await.unwrap();
    shorten_busy_timeout(&st.db).await;
    let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), quota::take_health_events(&st.db))
        .await
        .expect("writer acquisition must stop at its hard limit");
    assert!(
        result.is_err(),
        "a held writer must return an error, not hang"
    );
    writer.rollback().await.unwrap();
}

#[tokio::test]
async fn purge_gives_up_quickly_for_a_held_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = tokomotive::db::connect(&format!("sqlite://{}/db", dir.path().display()))
        .await
        .unwrap();
    let writer = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(400),
        tokomotive::cache::maintain(&db, &Default::default()),
    )
    .await;
    writer.rollback().await.unwrap();
    assert!(
        result
            .expect("purges must not spend the request writer budget")
            .is_err()
    );
}

#[tokio::test]
async fn reenable_waits_for_a_contended_writer() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&format!(
        "[server]\ndatabase='sqlite://{}/db.sqlite'\n[[providers]]\nname='test'\nkind='openai'\nbase_url='http://127.0.0.1:1'\napi_key='a'",
        dir.path().display()
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    quota::record(&st.db, "test", "default", 401, &HeaderMap::new(), 1)
        .await
        .unwrap();
    shorten_busy_timeout(&st.db).await;
    let mut writer = st.db.begin().await.unwrap();
    sqlx::query("UPDATE provider_health SET observed_at=observed_at")
        .execute(&mut *writer)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        writer.commit().await.unwrap();
    });
    tokio::time::timeout(
        Duration::from_secs(20),
        quota::reenable(&st, "test", "default"),
    )
    .await
    .expect("reenable finishes after the writer releases the lock")
    .unwrap();
    release.await.unwrap();
    assert_eq!(
        quota::accounts(&st.db, None, 10).await.unwrap()[0].state,
        "ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_provider_requests_do_not_lose_ledger_entries() {
    concurrent_requests(8, "simple").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_connection_pool_with_concurrent_proxy_requests_and_purge_does_not_deadlock() {
    concurrent_requests(2, "full").await;
}

async fn concurrent_requests(pool_size: u32, mode: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let responses_ready = std::sync::Arc::new(tokio::sync::Barrier::new(25));
    let ready = responses_ready.clone();
    let upstream = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().route("/v1/chat/completions", post(move || {
            let ready = ready.clone();
            async move {
            ready.wait().await;
            ([ ("x-ratelimit-remaining-requests", "100") ], Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}})))
            }
        }))).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database='sqlite://{}/db.sqlite'
mode='{mode}'
[[providers]]
name='first'
kind='openai'
base_url='{base}'
api_key='a'
[[providers]]
name='second'
kind='openai'
base_url='{base}'
api_key='b'
[[users]]
name='alice'
keys=['sk-test']
"#,
        dir.path().display()
    ))
    .unwrap();
    let db = tokomotive::db::connect_with_pool_size(&cfg.server.database, pool_size)
        .await
        .unwrap();
    tokomotive::db::seed(&db, &cfg).await.unwrap();
    let st = tokomotive::state::AppState::new(&cfg, db, None)
        .await
        .unwrap();
    sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO response_cache(key,body,body_bytes,created_ms,expires_ms) SELECT 'expired-'||x,zeroblob(16384),16384,0,0 FROM n")
        .execute(&st.db).await.unwrap();
    shorten_busy_timeout(&st.db).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut writer = st.db.begin().await.unwrap();
    sqlx::query("UPDATE virtual_keys SET last_used_at=last_used_at")
        .execute(&mut *writer)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        // Do not start the contention window until all upstreams can respond.
        responses_ready.wait().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        writer.commit().await.unwrap();
    });
    let purge_db = st.db.clone();
    let purge = tokio::spawn(async move {
        for _ in 0..40 {
            let _ = tokomotive::cache::maintain(&purge_db, &Default::default()).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let client = reqwest::Client::new();
    let mut requests = tokio::task::JoinSet::new();
    for i in 0..24 {
        let client = client.clone();
        let url = url.clone();
        requests.spawn(async move {
            let provider = if i % 2 == 0 { "first" } else { "second" };
            let response = client.post(format!("{url}/v1/chat/completions"))
                .bearer_auth("sk-test")
                .json(&json!({"model":format!("{provider}/model-{i}"),"messages":[{"role":"user","content":"hi"}]}))
                .send().await.unwrap();
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap();
        });
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
        st.flush().await;
        purge.await.unwrap();
    })
    .await
    .expect("requests and tracked writes finish");
    release.await.unwrap();
    let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(
        entries, 24,
        "every completed request has a Ledger Entry after flush"
    );
    let unique_requests: i64 =
        sqlx::query_scalar("SELECT COUNT(DISTINCT requested_model) FROM ledger")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(
        unique_requests, 24,
        "a duplicate must not hide a lost entry"
    );
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(attempts, 24);
    let health: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_health")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(health, 2);
    let snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_quota")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(snapshots, 2);
    proxy.abort();
    upstream.abort();
}
