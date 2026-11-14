//! Real HTTP Proxy and database with a local Cohere v2 mock.
use axum::{
    Json,
    body::Body,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
async fn mock(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    assert_eq!(headers["authorization"], "Bearer private-cohere-key");
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(body["model"], "command-a");
    assert!(body.get("stream_options").is_none());
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"],
        "weather"
    );
    assert_eq!(body["messages"][2]["tool_call_id"], "call-old");
    let usage = json!({"billed_units":{"input_tokens":5,"output_tokens":26},"tokens":{"input_tokens":71,"output_tokens":30}});
    let call =
        json!({"id":"call-new","type":"function","function":{"name":"weather","arguments":"{}"}});
    if body["stream"] == true {
        let events = [
            json!({"type":"message-start","id":"co-1","delta":{"message":{"role":"assistant"}}}),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"Hello"}}}}),
            json!({"type":"tool-call-start","index":0,"delta":{"message":{"tool_calls":call}}}),
            json!({"type":"tool-call-end","index":0}),
            json!({"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":usage}}),
        ];
        let wire = events
            .into_iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect::<String>();
        let chunks = wire
            .as_bytes()
            .chunks(3)
            .map(|c| Ok::<_, std::io::Error>(c.to_vec()))
            .collect::<Vec<_>>();
        return (
            [("content-type", "text/event-stream")],
            Body::from_stream(futures_util::stream::iter(chunks)),
        )
            .into_response();
    }
    Json(json!({"id":"co-1","finish_reason":"TOOL_CALL","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}],"tool_calls":[call]},"usage":usage})).into_response()
}
#[tokio::test]
async fn cohere_json_and_stream_both_clients_tools_and_accounting() {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(upstream, axum::Router::new().route("/v2/chat", post(mock)))
            .await
            .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg: tokomotive::config::Config = toml::from_str(&format!(
        r#"
[server]
database="sqlite://{}/db.sqlite"
[[providers]]
name="cohere"
kind="cohere"
base_url="http://{address}/v2"
api_key="private-cohere-key"
[[routes]]
name="route"
deployments=[{{provider="cohere",model="command-a"}}]
[[users]]
name="alice"
keys=["sk-test"]
"#,
        dir.path().display()
    ))
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    for messages in [false, true] {
        for stream in [false, true] {
            let history = if messages {
                json!([{"role":"user","content":"Hi"},{"role":"assistant","content":[{"type":"tool_use","id":"call-old","name":"weather","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-old","content":"sunny"}]}])
            } else {
                json!([{"role":"user","content":"Hi"},{"role":"assistant","tool_calls":[{"id":"call-old","type":"function","function":{"name":"weather","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call-old","content":"sunny"}])
            };
            let path = if messages {
                "messages"
            } else {
                "chat/completions"
            };
            let r=client.post(format!("{url}/v1/{path}")).bearer_auth("sk-test").header("x-api-key","client-forged").json(&json!({"model":"route","messages":history,"max_tokens":40,"stream":stream,"stream_options":{"include_usage":true}})).send().await.unwrap();
            assert_eq!(r.status(), 200);
            let text = r.text().await.unwrap();
            assert!(text.contains("Hello") && text.contains("weather"), "{text}");
            if stream {
                assert!(
                    text.contains(if messages { "message_stop" } else { "[DONE]" }),
                    "{text}"
                );
            } else {
                let body: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    body["usage"][if messages {
                        "input_tokens"
                    } else {
                        "prompt_tokens"
                    }],
                    5
                );
            }
        }
    }
    st.flush().await;
    let usage: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT input,output,estimated FROM ledger")
        .fetch_all(&st.db)
        .await
        .unwrap();
    assert_eq!(usage, vec![(5, 26, 0); 4]);
    task.abort();
    proxy.abort();
}
