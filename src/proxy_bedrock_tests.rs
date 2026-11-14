//! TLS mock keeps the production AWS host and signature boundary unchanged.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, rustls};
fn frame(event: &str, body: Value) -> Vec<u8> {
    let mut h = Vec::new();
    for (n, v) in [(":message-type", "event"), (":event-type", event)] {
        h.push(n.len() as u8);
        h.extend(n.as_bytes());
        h.push(7);
        h.extend((v.len() as u16).to_be_bytes());
        h.extend(v.as_bytes());
    }
    let p = body.to_string();
    let mut out = Vec::from(((16 + h.len() + p.len()) as u32).to_be_bytes());
    out.extend((h.len() as u32).to_be_bytes());
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out.extend(h);
    out.extend(p.as_bytes());
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out
}
#[tokio::test]
async fn bedrock_real_proxy_signed_json_binary_stream_and_auth_status() {
    let host = "bedrock-runtime.us-east-1.amazonaws.com";
    let cert = rcgen::generate_simple_self_signed(vec![host.into()]).unwrap();
    let root = reqwest::Certificate::from_der(cert.cert.der()).unwrap();
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = TlsAcceptor::from(std::sync::Arc::new(tls));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let accept = accept.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut socket = socket;
                let mut connect = Vec::new();
                while !connect.ends_with(b"\r\n\r\n") {
                    let mut b = [0];
                    socket.read_exact(&mut b).await.unwrap();
                    connect.push(b[0]);
                    assert!(connect.len() < 4096);
                }
                assert!(
                    std::str::from_utf8(&connect)
                        .unwrap()
                        .starts_with("CONNECT bedrock-runtime.us-east-1.amazonaws.com:443 ")
                );
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let mut socket = accept.accept(socket).await.unwrap();
                let mut raw = Vec::new();
                let (end, len) = loop {
                    let mut b = [0u8; 4096];
                    let n = socket.read(&mut b).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    raw.extend(&b[..n]);
                    if let Some(e) = raw.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&raw[..e]).unwrap();
                        let len = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (e + 4, len);
                    }
                };
                while raw.len() < end + len {
                    let mut b = [0u8; 4096];
                    let n = socket.read(&mut b).await.unwrap();
                    assert!(n > 0);
                    raw.extend(&b[..n]);
                }
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let headers = std::str::from_utf8(&raw[..end]).unwrap();
                assert!(headers.contains("AWS4-HMAC-SHA256 Credential=ACCESS/"));
                assert!(headers.contains("x-amz-security-token: SESSION"));
                assert!(!headers.contains("client-forged"));
                assert!(!headers.contains("SECRET"));
                let body: Value = serde_json::from_slice(&raw[end..end + len]).unwrap();
                assert_eq!(body["messages"][0]["content"][0]["text"], "Hi");
                let status = if headers.contains("/model/denied/") {
                    403
                } else if headers.contains("/model/unauthorized/") {
                    401
                } else {
                    200
                };
                let stream = headers.lines().next().unwrap().contains("converse-stream");
                let usage = json!({"inputTokens":10,"outputTokens":3,"cacheReadInputTokens":20,"cacheWriteInputTokens":5});
                let (mime, reply) = if status != 200 {
                    ("application/json", b"{\"message\":\"denied\"}".to_vec())
                } else if stream {
                    let events = [
                        ("messageStart", json!({"role":"assistant"})),
                        (
                            "contentBlockDelta",
                            json!({"contentBlockIndex":0,"delta":{"text":"Hello"}}),
                        ),
                        (
                            "contentBlockStart",
                            json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"call","name":"weather"}}}),
                        ),
                        (
                            "contentBlockDelta",
                            json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{}"}}}),
                        ),
                        ("contentBlockStop", json!({"contentBlockIndex":1})),
                        ("messageStop", json!({"stopReason":"tool_use"})),
                        ("metadata", json!({"usage":usage})),
                    ];
                    (
                        "application/vnd.amazon.eventstream",
                        events.into_iter().flat_map(|(e, b)| frame(e, b)).collect(),
                    )
                } else {
                    ("application/json",json!({"output":{"message":{"role":"assistant","content":[{"text":"Hello"},{"toolUse":{"toolUseId":"call","name":"weather","input":{}}}]}},"stopReason":"tool_use","usage":usage}).to_string().into_bytes())
                };
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.len()).as_bytes()).await.unwrap();
                for b in reply.chunks(5) {
                    socket.write_all(b).await.unwrap();
                }
                socket.shutdown().await.unwrap();
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg: crate::config::Config = toml::from_str(&format!(
        r#"
[server]
database="sqlite://{}/db.sqlite"
[[providers]]
name="aws"
kind="bedrock"
base_url="https://bedrock-runtime.us-east-1.amazonaws.com"
[[users]]
name="alice"
keys=["sk-test"]
"#,
        dir.path().display()
    ))
    .unwrap();
    let st = crate::init(&cfg, None).await.unwrap();
    let creds = crate::sigv4::Credentials::new("ACCESS", "SECRET", Some("SESSION".into()));
    sqlx::query("UPDATE providers SET bedrock_credentials=? WHERE name='aws'")
        .bind(
            st.secret_key
                .encrypt(&serde_json::to_string(&creds).unwrap())
                .unwrap(),
        )
        .execute(&st.db)
        .await
        .unwrap();
    // A local CONNECT proxy terminates TLS; it never connects to AWS.
    st.set_test_http(
        reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::https(format!("http://{addr}")).unwrap())
            .add_root_certificate(root)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", l.local_addr().unwrap());
    let router = crate::router(st.clone());
    let task = tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    let http = reqwest::Client::new();
    for messages in [false, true] {
        for stream in [false, true] {
            let endpoint = if messages {
                "messages"
            } else {
                "chat/completions"
            };
            let r=http.post(format!("{proxy}/v1/{endpoint}")).bearer_auth("sk-test").header("x-amz-security-token","client-forged").json(&json!({"model":"aws/arn:aws:bedrock:us-east-1:123456789012:inference-profile/test","messages":[{"role":"user","content":"Hi"}],"max_tokens":10,"stream":stream,"stream_options":{"include_usage":true}})).send().await.unwrap();
            assert_eq!(r.status(), 200);
            let text = r.text().await.unwrap();
            assert!(text.contains("Hello") && text.contains("weather"), "{text}");
            if stream {
                assert!(
                    text.contains(if messages { "message_stop" } else { "[DONE]" }),
                    "{text}"
                );
            } else {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    v["usage"][if messages {
                        "input_tokens"
                    } else {
                        "prompt_tokens"
                    }],
                    if messages { 10 } else { 35 }
                );
            }
        }
    }
    for (model, status) in [("denied", 403), ("unauthorized", 401)] {
        let before = count.load(std::sync::atomic::Ordering::SeqCst);
        let r=http.post(format!("{proxy}/v1/chat/completions")).bearer_auth("sk-test").json(&json!({"model":format!("aws/{model}"),"messages":[{"role":"user","content":"Hi"}]})).send().await.unwrap();
        assert_eq!(r.status(), status);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst) - before, 1);
    }
    st.flush().await;
    let rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT input,output,cache_read,cache_write_5m FROM ledger WHERE status=200",
    )
    .fetch_all(&st.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| *r == (10, 3, 20, 5)));
    task.abort();
    upstream.abort();
}
