//! Real WAL contention: busy-handler expiry must not discard accounting writes.
use axum::{Json, http::HeaderMap, routing::post};
use serde_json::json;
use std::time::Duration;
use tokomotive::{config::Config, quota};

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
mode='simple'
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
    let st = tokomotive::init(&cfg, None).await.unwrap();
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
