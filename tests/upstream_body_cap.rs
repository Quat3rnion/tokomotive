//! A non-streaming upstream body larger than the cap is refused with 502 and
//! recorded, instead of being read into memory without a bound.

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use tokomotive::config::Config;

#[tokio::test]
async fn oversized_non_streaming_upstream_body_is_refused() {
    let upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            // 80 MiB of JSON-ish bytes, streamed so the mock does not hold it.
            let chunk = vec![b' '; 1024 * 1024];
            let stream = futures_util::stream::iter(
                (0..80).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
            );
            let r: Response = (
                [("content-type", "application/json")],
                Body::from_stream(stream),
            )
                .into_response();
            r
        }),
    );
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let u = format!("http://{}", ul.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database = "sqlite://{}/db.sqlite"
[[providers]]
name = "p"
kind = "openai"
base_url = "{u}"
api_key = "k"
[[users]]
name = "u"
keys = ["vk-cap"]
"#,
        dir.path().display()
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let r = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("vk-cap")
        .json(&serde_json::json!({"model": "p/m", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    st.flush().await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE status = 502")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(n, 1, "the refused request is still recorded");
}
