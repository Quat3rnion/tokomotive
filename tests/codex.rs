use serde_json::json;
use tokomotive::config::Config;
use tokomotive::translate_responses;
#[path = "support/codex.rs"]
#[allow(dead_code)] // Shared fixture controls are used by different integration suites.
mod mock_codex;

#[tokio::test]
async fn codex_model_requests_are_confined_translated_accounted_and_refresh_once() {
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints},
        state::AppState,
    };
    let mock = mock_codex::MockCodex::start().await;
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
[[users]]
name='denied'
keys=['vk-denied']
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::new();
    for (path, body, key, status) in [
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}),
            "vk-denied",
            403,
        ),
        (
            "/v1/embeddings",
            json!({"model":"gpt-test","input":"hi"}),
            "vk-allowed",
            400,
        ),
        (
            "/v1/organizations",
            json!({"model":"gpt-test"}),
            "vk-allowed",
            400,
        ),
    ] {
        let r = c
            .post(format!("{base}{path}"))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status, "{path}");
    }
    assert!(mock.requests.lock().unwrap().is_empty());
    // Raw TCP retains dot segments that URL libraries would normalize away.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for path in [
        "/v1/../api/profile/responses",
        "/v1/%2e%2e/api/profile/responses",
        "/v1/responses/../../api/profile",
        "/v1/responses/batches",
        "/v1/./chat/completions",
    ] {
        let addr = base.trim_start_matches("http://");
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(format!("POST {path} HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer vk-allowed\r\ncontent-length: 20\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{{\"model\":\"gpt-test\"}}").as_bytes()).await.unwrap();
        let mut out = Vec::new();
        socket.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 400"), "{path}: {text}");
    }
    assert!(mock.requests.lock().unwrap().is_empty());
    *mock.unauthorized.lock().unwrap() = true;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[{"role":"system","content":"Be concise"},{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"gpt-test","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"gpt-test","max_tokens":30,"messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"gpt-test","max_tokens":30,"stream":true,"messages":[{"role":"user","content":"hi"}]}),
        ),
        ("/v1/responses", json!({"model":"gpt-test","input":"hi"})),
        (
            "/v1/responses",
            json!({"model":"gpt-test","stream":true,"input":"hi"}),
        ),
    ] {
        let r = c
            .post(format!("{base}{path}"))
            .bearer_auth("vk-allowed")
            .header("originator", "evil")
            .header("chatgpt-account-id", "evil")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let content_type = r.headers()["content-type"].to_str().unwrap().to_owned();
        let text = r.text().await.unwrap();
        assert!(text.contains("Hello"), "{path}: {text}");
        if body["stream"] == true {
            assert!(content_type.starts_with("text/event-stream"));
        } else {
            assert!(content_type.starts_with("application/json"));
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            if path.ends_with("messages") {
                assert_eq!(v["type"], "message");
                assert_eq!(v["usage"]["cache_read_input_tokens"], 4);
            } else if path.ends_with("completions") {
                assert_eq!(v["object"], "chat.completion");
                assert_eq!(v["usage"]["prompt_tokens"], 10);
            } else {
                assert_eq!(v["id"], "resp_1");
            }
        }
    }
    st.flush().await;
    let rows: Vec<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT input,output,cache_read,cost_micros,price_micros,subscription FROM ledger",
    )
    .fetch_all(&st.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 6);
    assert!(
        rows.iter()
            .all(|r| r.0 == 6 && r.1 == 3 && r.2 == 4 && r.3 == 0 && r.4 > 0 && r.5 == 1),
        "{rows:?}"
    );
    let seen = mock.requests.lock().unwrap();
    assert_eq!(seen.len(), 7);
    for (path, headers, body) in seen.iter() {
        assert_eq!(path, "/backend-api/codex/responses");
        assert_eq!(headers["originator"], "codex_cli_rs");
        assert_eq!(headers["chatgpt-account-id"], "acct");
        assert!(!headers["session-id"].is_empty());
        assert!(!headers["version"].is_empty());
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(
            body["include"]
                .as_array()
                .unwrap()
                .contains(&json!("reasoning.encrypted_content"))
        );
    }
    assert_eq!(mock.tokens.lock().unwrap().len(), 2);
    server.abort();
}

#[test]
fn responses_sse_errors_and_truncation_never_report_success() {
    for event in [
        json!({"type":"error","error":{"message":"private upstream body"}}),
        json!({"type":"response.failed","response":{"error":{"message":"private upstream body"}}}),
        json!({"type":"response.output_text.delta","delta":"partial"}),
    ] {
        for messages in [false, true] {
            let mut decoder = translate_responses::ResponseStream::new("gpt-test", messages);
            let mut bytes = decoder.feed(format!("data: {event}\n\n").as_bytes());
            bytes.extend(decoder.finish());
            let text = String::from_utf8(bytes).unwrap();
            assert!(decoder.failed);
            assert!(text.contains("error"));
            assert!(!text.contains("private upstream body"));
            if messages {
                assert!(!text.contains("message_stop"));
            }
        }
    }
}

#[test]
fn malformed_tools_return_a_translation_error() {
    assert!(
        translate_responses::request(
            &json!({"messages":[],"tools":[{"type":"function","function":"invalid"}]})
        )
        .is_err()
    );
}

#[test]
fn responses_sse_ignores_non_object_data() {
    for data in ["[1]", "3", "null"] {
        for named in [true, false] {
            let mut decoder = translate_responses::ResponseStream::new("gpt-test", false);
            let event = if named {
                format!("event: x\ndata: {data}\n\n")
            } else {
                format!("data: {data}\n\n")
            };
            assert!(decoder.feed(event.as_bytes()).is_empty());
        }
    }
}

#[test]
fn responses_tool_count_is_capped() {
    for messages in [false, true] {
        let mut decoder = translate_responses::ResponseStream::new("gpt-test", messages);
        for index in 0..128 {
            let event = json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":"c","name":"f","arguments":""}});
            assert!(
                !String::from_utf8(decoder.feed(format!("data: {event}\n\n").as_bytes()))
                    .unwrap()
                    .contains("error")
            );
        }
        let event = json!({"type":"response.output_item.added","output_index":128,"item":{"type":"function_call","call_id":"c","name":"f","arguments":""}});
        let out = String::from_utf8(decoder.feed(format!("data: {event}\n\n").as_bytes())).unwrap();
        assert!(
            out.contains("error"),
            "129th tool must terminate with an error"
        );
        assert!(decoder.failed);
        assert!(decoder.finish().is_empty());
    }
}

#[test]
fn messages_tool_buffers_are_capped_across_events() {
    use tokomotive::translate::{Direction, StreamTranslator};
    for count_limit in [false, true] {
        let mut decoder = StreamTranslator::new(Direction::MessagesToChat, "gpt-test");
        let mut failed = false;
        for index in 0..129 {
            let args = if count_limit {
                "".into()
            } else {
                "x".repeat(1024 * 1024)
            };
            let event = json!({"choices":[{"delta":{"tool_calls":[{"index":if count_limit {index} else {0},"id":"c","function":{"name":"f","arguments":args}}]}}]});
            let out =
                String::from_utf8(decoder.feed(format!("data: {event}\n\n").as_bytes())).unwrap();
            if out.contains("error") {
                failed = true;
                break;
            }
        }
        assert!(
            failed,
            "response-wide tool buffer limit must emit an error (count={count_limit})"
        );
        assert!(decoder.finish().is_empty());
    }
}

#[test]
fn responses_tool_arguments_are_capped_across_events() {
    for messages in [false, true] {
        let mut decoder = translate_responses::ResponseStream::new("gpt-test", messages);
        decoder.feed(b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c\",\"name\":\"f\"}}\n\n");
        let event = json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"x".repeat(1024 * 1024)});
        let event = format!("data: {event}\n\n");
        let mut failed = false;
        for _ in 0..17 {
            let out = String::from_utf8(decoder.feed(event.as_bytes())).unwrap();
            if out.contains("error") {
                failed = true;
                break;
            }
        }
        assert!(failed, "response-wide tool arguments must be capped");
        assert!(decoder.failed);
    }
}

#[test]
fn complete_responses_events_have_the_same_size_limit_as_partial_events() {
    let event = json!({"type":"response.output_text.delta","delta":"x".repeat(4*1024*1024)});
    let mut decoder = translate_responses::ResponseStream::new("gpt-test", false);
    let output = decoder.feed(format!("data: {event}\n\n").as_bytes());
    assert!(decoder.failed);
    assert!(output.len() < 1024);
}

#[test]
fn chat_request_maps_messages_tools_and_options_to_responses() {
    let out = translate_responses::request(&json!({"model":"gpt-test","stream":false,"max_tokens":42,"temperature":0.4,
        "messages":[{"role":"system","content":"Be concise."},{"role":"developer","content":"Use tools."},
            {"role":"user","content":"hi"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"sunny"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}]})).unwrap();
    assert_eq!(out["instructions"], "Be concise.\nUse tools.");
    assert_eq!(
        out["input"][0],
        json!({"role":"user","content":[{"type":"input_text","text":"hi"}]})
    );
    assert_eq!(
        out["input"][1],
        json!({"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"})
    );
    assert_eq!(
        out["input"][2],
        json!({"type":"function_call_output","call_id":"call_1","output":"sunny"})
    );
    assert_eq!(out["tools"][0]["name"], "weather");
    assert_eq!(out["max_output_tokens"], 42);
    assert_eq!(out["temperature"], 0.4);
    assert_eq!(out["stream"], false);
}

#[test]
fn codex_is_a_separate_subscription_auth_type() {
    let cfg: Config = toml::from_str(
        r#"
[[providers]]
name='legacy'
kind='openai'
auth='codex'
base_url='https://chatgpt.com/backend-api/codex'
"#,
    )
    .unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.providers[0].auth.as_str(), "codex");
}

fn completed() -> serde_json::Value {
    json!({"id":"resp_1","model":"gpt-test","status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":"Hello"}]},
        {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Paris\"}"}],
        "usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}})
}

#[test]
fn responses_reply_maps_text_calls_and_cached_usage() {
    let out = translate_responses::response(&completed());
    assert_eq!(out["choices"][0]["message"]["content"], "Hello");
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_1"
    );
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        out["usage"],
        json!({"prompt_tokens":10,"completion_tokens":3,"total_tokens":13,"prompt_tokens_details":{"cached_tokens":4}})
    );
}

#[test]
fn responses_sse_maps_text_and_argument_deltas_at_every_chunk_boundary() {
    let events = [
        json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}),
        json!({"type":"response.output_text.delta","delta":"Hello"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"weather","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"{\"city\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":"\"Paris\"}"}),
        json!({"type":"response.completed","response":completed()}),
    ];
    let source: String = events
        .iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect();
    let mut expected = None;
    for size in 1..=source.len() {
        let mut stream =
            translate_responses::ResponseStream::new("gpt-test", false).with_usage(true);
        let mut out = Vec::new();
        for chunk in source.as_bytes().chunks(size) {
            out.extend(stream.feed(chunk));
        }
        out.extend(stream.finish());
        let text = String::from_utf8(out).unwrap();
        if let Some(expected) = &expected {
            assert_eq!(&text, expected);
        } else {
            assert!(
                text.contains("Hello")
                    && text.contains("call_1")
                    && text.contains("prompt_tokens")
                    && text.ends_with("data: [DONE]\n\n")
            );
            expected = Some(text);
        }
    }
}

#[tokio::test]
async fn device_panel_is_admin_same_origin_and_exposes_no_device_secret() {
    use tokomotive::{db, oauth::OAuthEndpoints, state::AppState};
    let mock = mock_codex::MockCodex::start().await;
    let cfg:Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='legacy'\nkind='openai'\nauth='codex'\nbase_url='https://chatgpt.com/backend-api/codex'").unwrap();
    let database = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&database, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        database,
        None,
        OAuthEndpoints {
            issuer: mock.base.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let path = format!("{base}/admin/providers/legacy/oauth/start");
    assert!(
        c.post(&path)
            .send()
            .await
            .unwrap()
            .status()
            .is_redirection()
    );
    c.post(format!("{base}/setup"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    let r = c
        .post(format!("{base}/admin/login"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(
        c.post(&path)
            .header("cookie", &cookie)
            .header("origin", "https://evil.test")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let r = c
        .post(&path)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(
        html.contains("ABCD-12345")
            && html.contains("data-copy=\"ABCD-12345\"")
            && html.contains("opens in a new tab"),
        "{html}"
    );
    assert!(!html.contains("device-secret"));
    let status_path = html
        .split("hx-post=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let r = c
        .post(format!("{base}{status_path}"))
        .header("cookie", &cookie)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Waiting"));
    use sha2::Digest;
    *mock.poll.lock().unwrap() = (
        axum::http::StatusCode::OK,
        json!({"authorization_code":"ok","code_verifier":"verifier","code_challenge":base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD,sha2::Sha256::digest(b"verifier"))}),
    );
    // Wait for the manager's background poll, not a browser-side token poll.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let r = c
        .post(format!("{base}{status_path}"))
        .header("cookie", &cookie)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers().get("hx-redirect").unwrap(), "/admin/providers");
    let paste = c
        .post(format!("{base}/admin/providers/legacy/oauth/paste"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        paste.contains("Paste")
            && paste.contains("<code class=\"url\">http://127.0.0.1:1455/auth/callback</code>")
    );
    server.abort();
}

/// Local mock fixture for the required browser checks. No real account is used.
#[tokio::test]
#[ignore = "interactive Playwright fixture; run this test binary with --ignored --nocapture"]
async fn visual_signin_fixture() {
    use tokomotive::{db, oauth::OAuthEndpoints, state::AppState};
    let mock = mock_codex::MockCodex::start().await;
    let cfg: Config = toml::from_str(
        r#"
[server]
database='sqlite::memory:'
[[providers]]
name='legacy'
kind='openai'
auth='codex'
base_url='https://chatgpt.com/backend-api/codex'
[[providers]]
name='sharing'
kind='openai'
auth='chatgpt'
base_url='https://api.openai.com'
[[providers]]
name='claude'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
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
            claude_authorize: format!("{}/authorize", mock.base),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:42788")
        .await
        .unwrap();
    println!("Mock sign-in fixture: http://127.0.0.1:42788/setup");
    axum::serve(listener, tokomotive::router(st)).await.unwrap();
}
