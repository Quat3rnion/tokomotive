use serde_json::json;
use std::time::Duration;
use tokomotive::config::Config;
#[path = "support/codex.rs"]
#[allow(dead_code)] // Shared fixture controls are used by different integration suites.
mod mock_codex;

async fn codex_state(mock: &mock_codex::MockCodex) -> tokomotive::state::AppState {
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    let cfg: Config = toml::from_str(
        r#"
[server]
database='sqlite::memory:'
[[providers]]
name='legacy'
kind='openai'
auth='codex'
base_url='https://chatgpt.com/backend-api/codex'
[[models]]
name='gpt-test'
provider='legacy'
cost={input=2,output=4}
price={input=3,output=5}
[[users]]
name='allowed'
keys=['vk-allowed']
key_allowed_providers=['legacy']
budget_usd=1
key_tpm_limit=13
"#,
    )
    .unwrap();
    let database = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&database, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        database,
        None,
        OAuthEndpoints {
            issuer: mock.base.clone(),
            codex_resource: format!("{}/backend-api/codex", mock.base),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a = st.oauth.start("legacy", Flow::Codex).await.unwrap();
    let state = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    st.oauth
        .finish(
            "legacy",
            Flow::Codex,
            &format!("http://127.0.0.1:1455/auth/callback?code=ok&state={state}"),
        )
        .await
        .unwrap();
    st
}

async fn serve(st: tokomotive::state::AppState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, tokomotive::router(st)).await.unwrap();
    });
    (base, task)
}

#[tokio::test]
async fn aborted_codex_json_collection_keeps_usage_and_reservation() {
    use tokio::io::AsyncWriteExt;
    let mock = mock_codex::MockCodex::start().await;
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    *mock.completion_gate.lock().unwrap() = Some(gate.clone());
    let st = codex_state(&mock).await;
    let (base, server) = serve(st.clone()).await;
    let addr = base.trim_start_matches("http://");
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = json!({"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}).to_string();
    socket.write_all(format!("POST /v1/chat/completions HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer vk-allowed\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), mock.body_started.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let user: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name='allowed'")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert!(st.reserved(user) > 0);
    drop(socket);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let reserved_after_abort = st.reserved(user);
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), st.flush())
        .await
        .unwrap();
    let rows: Vec<(i64, i64, i64, i64)> =
        sqlx::query_as("SELECT input,output,cache_read,client_aborted FROM ledger")
            .fetch_all(&st.db)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![(6, 3, 4, 1)],
        "client cancellation must not cancel upstream accounting"
    );
    assert!(
        reserved_after_abort > 0,
        "reservation must survive the client"
    );
    assert_eq!(st.reserved(user), 0);
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth("vk-allowed")
        .json(&json!({"model":"gpt-test","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        429,
        "aborted request usage must count against TPM"
    );
    server.abort();
}

#[tokio::test]
async fn tracked_task_panic_does_not_block_flush() {
    let cfg: Config = toml::from_str("[server]\ndatabase='sqlite::memory:'").unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.spawn_tracked(async move {
        let _ = tx.send(());
        panic!("test tracked task panic");
    });
    rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), st.flush())
        .await
        .expect("panicking task must release the pending counter");
}

#[tokio::test]
async fn codex_redirect_never_reaches_the_second_listener() {
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received = seen.clone();
    let app = Router::new().route(
        "/sink",
        post(move |headers: HeaderMap| {
            received.lock().unwrap().push(headers);
            async { "redirect followed" }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}/sink", listener.local_addr().unwrap());
    let sink = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mock = mock_codex::MockCodex::start().await;
    let mut headers = HeaderMap::new();
    headers.insert("location", target.parse().unwrap());
    *mock.reply.lock().unwrap() = Some((StatusCode::TEMPORARY_REDIRECT, headers, String::new()));
    let st = codex_state(&mock).await;
    let (base, server) = serve(st.clone()).await;
    for reload in [false, true] {
        if reload {
            st.reload_settings().await.unwrap();
        }
        let r = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("vk-allowed")
            .json(&json!({"model":"gpt-test","messages":[]}))
            .send()
            .await
            .unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "redirect target must receive no request, including bearer credentials"
        );
        assert_eq!(r.status(), 502, "redirect must be an upstream error");
    }
    assert!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, headers, _)| headers["authorization"] == "Bearer codex-access")
    );
    st.flush().await;
    let statuses: Vec<i64> = sqlx::query_scalar("SELECT status FROM ledger")
        .fetch_all(&st.db)
        .await
        .unwrap();
    assert_eq!(statuses, vec![502, 502]);
    server.abort();
    sink.abort();
}

#[tokio::test]
async fn tool_cap_stream_error_is_accounted_as_upstream_failure() {
    use axum::http::{HeaderMap, StatusCode};
    for path in ["chat/completions", "messages"] {
        let mock = mock_codex::MockCodex::start().await;
        let st = codex_state(&mock).await;
        let mut body = String::new();
        for index in 0..129 {
            let event = json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":"c","name":"f"}});
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str(&format!("data: {}\n\n",json!({"type":"response.completed","response":{"id":"r","model":"gpt-test","output":[],"usage":{"input_tokens":10,"output_tokens":3}}})));
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        *mock.reply.lock().unwrap() = Some((StatusCode::OK, headers, body));
        let (base, server) = serve(st.clone()).await;
        let r = reqwest::Client::new()
            .post(format!("{base}/v1/{path}"))
            .bearer_auth("vk-allowed")
            .json(&json!({"model":"gpt-test","stream":true,"max_tokens":30,"messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("error"));
        st.flush().await;
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT status,input,output FROM ledger")
            .fetch_all(&st.db)
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![(502, 10, 3)],
            "translation failure must not be accounted as success"
        );
        server.abort();
    }
}
