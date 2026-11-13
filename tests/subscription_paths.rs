//! A subscription bearer must only reach the model endpoints of its Provider.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::http::HeaderMap;
use axum::routing::{any, post};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokomotive::config::Config;
use tokomotive::db;
use tokomotive::oauth::{Flow, OAuthEndpoints};
use tokomotive::state::AppState;

/// Send a request with a raw path. HTTP clients normalize dot segments
/// before they send, so use a plain TCP stream.
async fn raw(addr: &str, method: &str, path: &str) -> u16 {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer vk-test\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let head = String::from_utf8_lossy(&out);
    head.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[tokio::test]
async fn subscription_bearer_never_leaves_the_model_endpoints() {
    // Every request that reaches the mock with a bearer, except /v1/messages.
    let leaked = Arc::new(Mutex::new(Vec::<String>::new()));
    let l = leaked.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        let app = Router::new()
            .route(
                "/token",
                post(|| async {
                    Json(json!({"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}))
                }),
            )
            .route(
                "/v1/messages",
                post(|| async {
                    Json(json!({"id":"m","model":"m","role":"assistant","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))
                }),
            )
            .fallback(any(move |uri: axum::http::Uri, headers: HeaderMap| {
                let l = l.clone();
                async move {
                    if headers.contains_key("authorization") {
                        l.lock().unwrap().push(uri.to_string());
                    }
                    Json(json!({"email":"owner@example.test"}))
                }
            }));
        axum::serve(listener, app).await.unwrap()
    });
    let cfg: Config = toml::from_str(
        r#"
[server]
database='sqlite::memory:'
[[providers]]
name='sub'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
[[models]]
name='m'
provider='sub'
cost={input=2,output=4}
[[users]]
name='user'
keys=['vk-test']
key_allowed_providers=['sub']
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
            claude_token: format!("{base}/token"),
            claude_resource: base,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a = st.oauth.start("sub", Flow::Claude).await.unwrap();
    let state: String = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    st.oauth
        .finish("sub", Flow::Claude, &format!("code#{state}"))
        .await
        .unwrap();
    // Sign-in reads the account profile once on the operator's behalf.
    // Only requests from Virtual Keys count below.
    leaked.lock().unwrap().clear();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // The normal model endpoint still works.
    assert_eq!(raw(&addr, "POST", "/v1/messages").await, 200);
    for (method, path) in [
        ("GET", "/v1/../api/oauth/profile"),
        ("GET", "/v1/%2e%2e/api/oauth/profile"),
        ("GET", "/v1/%2E%2E/api/oauth/profile"),
        ("POST", "/v1/../api/oauth/profile/messages"),
        ("POST", "/v1/./messages/../../api/oauth/profile/messages"),
        ("GET", "/v1/organizations"),
        ("POST", "/v1/messages/batches"),
    ] {
        let status = raw(&addr, method, path).await;
        assert!(
            (400..500).contains(&status),
            "{method} {path} returned {status}"
        );
    }
    st.flush().await;
    assert_eq!(*leaked.lock().unwrap(), Vec::<String>::new());
    server.abort();
    mock.abort();
}
