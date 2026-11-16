//! The authorized seam is the router HTTP interface, with SDK-shaped JSON and
//! an HTTP upstream. These tests include authentication, routing and accounting.
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokomotive::{config::Config, state::AppState};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn native_authentication_precedes_body_validation() {
    let (st, url, mut tasks) = setup(Router::new()).await;
    let http = reqwest::Client::new();
    for path in [
        "/v1beta/models/chat:generateContent",
        "/model/chat/converse",
        "/model/chat/invoke",
    ] {
        for body in ["{", "{}"] {
            let response = http
                .post(format!("{url}{path}"))
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401, "{path}: {body}");
            if path.starts_with("/model/") {
                assert_eq!(
                    response.headers()["x-amzn-errortype"],
                    "AccessDeniedException"
                );
            } else {
                let value: Value = response.json().await.unwrap();
                assert_eq!(value["error"]["status"], "UNAUTHENTICATED");
            }
        }
    }
    let (limited, task) = serve(
        Router::new()
            .fallback(tokomotive::inbound::handle)
            .layer(axum::extract::DefaultBodyLimit::max(16))
            .with_state(st),
    )
    .await;
    tasks.push(task);
    for path in [
        "/v1beta/models/chat:generateContent",
        "/model/chat/converse",
        "/model/chat/invoke",
    ] {
        let response = http
            .post(format!("{limited}{path}"))
            .bearer_auth("sk-native")
            .body("x".repeat(17))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413, "{path}");
    }
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn bedrock_sdk_converse_tool_round_trip_and_invoke() {
    let (_, url, tasks) = setup(Router::new().route("/v1/chat/completions", post(|Json(body): Json<Value>| async move {
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call-weather");
        assert!(body["messages"][1]["content"].is_null());
        assert_eq!(body["messages"][2]["tool_call_id"], "call-weather");
        assert!(body["messages"][2]["content"].as_str().unwrap().contains("sunny"));
        Json(json!({"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-next","type":"function","function":{"name":"next","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}))
    }))).await;
    let http = reqwest::Client::new();
    let response=http.post(format!("{url}/model/chat/converse")).bearer_auth("sk-native").json(&json!({"messages":[{"role":"user","content":[{"text":"Weather?"}]},{"role":"assistant","content":[{"toolUse":{"toolUseId":"call-weather","name":"weather","input":{"city":"Paris"}}}]},{"role":"user","content":[{"toolResult":{"toolUseId":"call-weather","content":[{"json":{"sunny":true}}]}}]}],"toolConfig":{"tools":[{"toolSpec":{"name":"weather","inputSchema":{"json":{"type":"object"}}}}]},"inferenceConfig":{"maxTokens":32}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let v: Value = response.json().await.unwrap();
    assert_eq!(
        v["output"]["message"]["content"][0]["toolUse"]["name"],
        "next"
    );
    assert_eq!(v["usage"]["inputTokens"], 8);
    let response=http.post(format!("{url}/model/chat/invoke")).bearer_auth("sk-native").json(&json!({"anthropic_version":"bedrock-2023-05-31","max_tokens":32,"messages":[{"role":"user","content":"Weather?"},{"role":"assistant","content":[{"type":"tool_use","id":"call-weather","name":"weather","input":{"city":"Paris"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-weather","content":"sunny"}]}]})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let v: Value = response.json().await.unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["usage"]["input_tokens"], 8);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_streams_google_sse_array_and_aws_crc_frames() {
    let (_, url, tasks)=setup(Router::new().route("/v1/chat/completions",post(||async {
        let chunks=[json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"weather","arguments":"{"}}]},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"city\":\"Paris\"}"}}]},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}})];
        let wire=chunks.iter().map(|v|format!("data: {v}\n\n")).collect::<String>()+"data: [DONE]\n\n";
        ([("content-type","text/event-stream")],axum::body::Body::from_stream(futures_util::stream::iter(wire.into_bytes().into_iter().map(|b|Ok::<_,std::io::Error>(vec![b])))))
    }))).await;
    let http = reqwest::Client::new();
    for suffix in ["?alt=sse", ""] {
        let r = http
            .post(format!(
                "{url}/v1/models/chat:streamGenerateContent{suffix}"
            ))
            .header("x-goog-api-key", "sk-native")
            .json(&json!({"contents":[{"role":"user","parts":[{"text":"Hi"}]}],"safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_ONLY_HIGH"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("x-tokomotive-safety-settings")
                .and_then(|v| v.to_str().ok()),
            Some("applied-only-to-gemini-upstreams")
        );
        let text = r.text().await.unwrap();
        let chunks: Vec<Value> = if suffix.is_empty() {
            serde_json::from_str(&text).unwrap()
        } else {
            text.lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0]["text"],
            "Hi"
        );
        assert!(chunks.iter().any(
            |v| v["candidates"][0]["content"]["parts"][0]["functionCall"]["args"]["city"]
                == "Paris"
        ));
        assert!(
            chunks
                .iter()
                .any(|v| v["usageMetadata"]["totalTokenCount"] == 13)
        );
    }
    let r = http
        .post(format!("{url}/model/chat/converse-stream"))
        .bearer_auth("sk-native")
        .json(&json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "application/vnd.amazon.eventstream"
    );
    let bytes = r.bytes().await.unwrap();
    let mut decoder = tokomotive::eventstream::Decoder::default();
    let frames = decoder.feed(&bytes).unwrap();
    decoder.finish().unwrap();
    assert_eq!(frames[0].headers[":event-type"], "messageStart");
    let data: Vec<Value> = frames
        .iter()
        .map(|f| serde_json::from_slice(&f.payload).unwrap())
        .collect();
    assert!(data.iter().any(|v| v["delta"]["text"] == "Hi"));
    assert!(
        data.iter()
            .any(|v| v["start"]["toolUse"]["toolUseId"] == "call-1")
    );
    assert!(data.iter().any(|v| v["usage"]["totalTokens"] == 13));
    let mut damaged = bytes.to_vec();
    damaged[12] ^= 1;
    assert!(
        tokomotive::eventstream::Decoder::default()
            .feed(&damaged)
            .is_err()
    );
    let r=http.post(format!("{url}/model/chat/invoke-with-response-stream")).bearer_auth("sk-native").json(&json!({"anthropic_version":"bedrock-2023-05-31","max_tokens":32,"messages":[{"role":"user","content":"Hi"}]})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "application/vnd.amazon.eventstream"
    );
    let mut decoder = tokomotive::eventstream::Decoder::default();
    let frames = decoder.feed(&r.bytes().await.unwrap()).unwrap();
    decoder.finish().unwrap();
    use base64::Engine;
    let chunks: Vec<Value> = frames
        .iter()
        .map(|f| {
            assert_eq!(f.headers[":event-type"], "chunk");
            let payload: Value = serde_json::from_slice(&f.payload).unwrap();
            serde_json::from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(payload["bytes"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(chunks[0]["type"], "message_start");
    assert!(chunks.iter().any(|v| v["delta"]["text"] == "Hi"));
    assert_eq!(chunks.last().unwrap()["type"], "message_stop");
    for task in tasks {
        task.abort();
    }
}

async fn setup(upstream: Router) -> (AppState, String, Vec<tokio::task::JoinHandle<()>>) {
    setup_kind(upstream, "openai").await
}
async fn setup_kind(
    upstream: Router,
    kind: &str,
) -> (AppState, String, Vec<tokio::task::JoinHandle<()>>) {
    let (base, task) = serve(upstream).await;
    let cfg: Config = toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\nmode='simple'\n[[providers]]\nname='p'\nkind='{kind}'\nbase_url='{base}'\napi_key='upstream-secret'\n[[routes]]\nname='chat'\ndeployments=[{{provider='p',model='m'}}]\ncache={{enabled=true,ttl_secs=60}}\n[[users]]\nname='alice'\nkeys=['sk-native']")).unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let (url, proxy) = serve(tokomotive::router(st.clone())).await;
    (st, url, vec![task, proxy])
}

#[tokio::test]
async fn gemini_native_passthrough_count_and_embeddings() {
    let app=Router::new().route("/v1beta/models/m:countTokens",post(|Json(v):Json<Value>|async move {assert_eq!(v["contents"][0]["parts"][0]["text"],"Hi");Json(json!({"totalTokens":7}))}))
        .route("/v1beta/models/m:generateContent",post(|Json(v):Json<Value>|async move {assert_eq!(v["safetySettings"][0]["threshold"],"BLOCK_ONLY_HIGH");Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":"STOP","safetyRatings":[{"category":"HARM_CATEGORY_HATE_SPEECH","probability":"NEGLIGIBLE"}]}],"modelVersion":"gemini-fixture","usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"totalTokenCount":9}}))}))
        .route("/v1beta/models/m:embedContent",post(|Json(v):Json<Value>|async move {assert_eq!(v["content"]["parts"][0]["text"],"Hi");Json(json!({"embedding":{"values":[0.1,0.2]}}))}))
        .route("/v1beta/models/m:batchEmbedContents",post(|Json(v):Json<Value>|async move {assert_eq!(v["requests"][0]["model"],"models/m");Json(json!({"embeddings":[{"values":[0.1,0.2]},{"values":[0.3,0.4]}]}))}));
    let (st, url, tasks) = setup_kind(app, "gemini").await;
    let http = reqwest::Client::new();
    for (op, body) in [
        (
            "countTokens",
            json!({"contents":[{"parts":[{"text":"Hi"}]}]}),
        ),
        (
            "generateContent",
            json!({"contents":[{"parts":[{"text":"Hi"}]}],"safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_ONLY_HIGH"}]}),
        ),
        ("embedContent", json!({"content":{"parts":[{"text":"Hi"}]}})),
        (
            "batchEmbedContents",
            json!({"requests":[{"model":"models/chat","content":{"parts":[{"text":"Hi"}]}},{"model":"models/chat","content":{"parts":[{"text":"There"}]}}]}),
        ),
    ] {
        let r = http
            .post(format!("{url}/v1beta/models/chat:{op}"))
            .header("x-goog-api-key", "sk-native")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{op}: {}", r.text().await.unwrap());
        // Repeat with Bearer to exercise native response cache hits as well.
        let r = http
            .post(format!("{url}/v1beta/models/chat:{op}"))
            .bearer_auth("sk-native")
            .json(&body)
            .send()
            .await
            .unwrap();
        let v: Value = r.json().await.unwrap();
        match op {
            "countTokens" => assert_eq!(v["totalTokens"], 7),
            "generateContent" => assert_eq!(v["modelVersion"], "gemini-fixture"),
            "embedContent" => assert_eq!(v["embedding"]["values"], json!([0.1, 0.2])),
            _ => assert_eq!(v["embeddings"].as_array().unwrap().len(), 2),
        }
    }
    st.flush().await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ledger WHERE endpoint='count_tokens' AND input=0 AND output=0",
    )
    .fetch_one(&st.db)
    .await
    .unwrap();
    assert_eq!(count, 2);
    let usage:Vec<(i64,i64)>=sqlx::query_as("SELECT input,estimated FROM ledger WHERE endpoint='embeddings' AND cache_hit=0 ORDER BY id").fetch_all(&st.db).await.unwrap();
    assert_eq!(usage, vec![(1, 1), (2, 1)]);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_guardrails_cache_format_scope_and_query_key_capture() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokomotive::guardrails::{Action, Direction, Kind, Rule};
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (st,url,tasks)=setup_kind(Router::new().route("/v1beta/models/m:generateContent",post(move |Json(v):Json<Value>|{let seen=seen.clone();async move {
        seen.fetch_add(1,Ordering::SeqCst);
        assert_eq!(v["contents"][0]["parts"][0]["text"],"[REDACTED:email]");
        Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"alice@example.com"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}))
    }})),"gemini").await;
    let rules = vec![Rule {
        name: "email".into(),
        action: Action::Redact,
        direction: Direction::Both,
        kind: Kind::Detector {
            detector: "email".into(),
        },
    }];
    let set = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('email',?)")
        .bind(serde_json::to_string(&rules).unwrap())
        .execute(&st.db)
        .await
        .unwrap()
        .last_insert_rowid();
    sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
        .bind(set)
        .execute(&st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET capture_bodies=1")
        .execute(&st.db)
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let body = json!({"contents":[{"parts":[{"text":"alice@example.com"}]}],"safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_ONLY_HIGH"}]});
    for _ in 0..2 {
        let r = http
            .post(format!(
                "{url}/v1beta/models/chat:generateContent?key=sk-native&trace=1"
            ))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(!text.contains("alice@example.com"));
        assert!(text.contains("[REDACTED:email]"));
        st.flush().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let entries = tokomotive::db::recent_ledger(&st.db, Default::default(), 10)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(!format!("{entry:?}").contains("sk-native"));
        if let Some(body) = tokomotive::observability::bodies(&st.db, &st.secret_key, entry.id)
            .await
            .unwrap()
        {
            assert!(!body.request.contains("sk-native"));
            assert!(!body.request.contains("alice@example.com"));
        }
        let attempts = tokomotive::observability::attempts(&st.db, entry.id)
            .await
            .unwrap();
        assert!(!format!("{attempts:?}").contains("sk-native"));
    }
    // A canonical Chat request with the same prompt must not reuse the native
    // cache entry (its safety configuration is not the same client contract).
    let r = http
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("sk-native")
        .json(&json!({"model":"chat","messages":[{"role":"user","content":"alice@example.com"}]}))
        .send()
        .await
        .unwrap();
    r.bytes().await.unwrap();
    assert!(calls.load(Ordering::SeqCst) > 1);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_error_shapes_auth_permissions_and_limits() {
    for gemini in [true, false] {
        let (st, url, tasks) = setup(Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(json!({"error":{"message":"Denied","type":"permission_error"}})),
                )
            }),
        ))
        .await;
        let http = reqwest::Client::new();
        for (model, status, name) in [
            ("missing", 404, "NOT_FOUND"),
            ("chat", 403, "PERMISSION_DENIED"),
        ] {
            if gemini {
                let r = http
                    .post(format!("{url}/v1beta/models/{model}:generateContent"))
                    .header("x-goog-api-key", "sk-native")
                    .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), status);
                let v: Value = r.json().await.unwrap();
                assert_eq!(v["error"]["code"], status);
                assert_eq!(v["error"]["status"], name);
            } else {
                let r = http
                    .post(format!("{url}/model/{model}/converse"))
                    .bearer_auth("sk-native")
                    .json(&json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), status);
                assert_eq!(
                    r.headers()["x-amzn-errortype"],
                    if status == 404 {
                        "ResourceNotFoundException"
                    } else {
                        "AccessDeniedException"
                    }
                );
            }
        }
        sqlx::query("UPDATE virtual_keys SET rpm_limit=0")
            .execute(&st.db)
            .await
            .unwrap();
        for (path, body, gemini) in [
            (
                "/v1beta/models/chat:generateContent",
                json!({"contents":[{"parts":[{"text":"Hi"}]}]}),
                true,
            ),
            (
                "/model/chat/converse",
                json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}),
                false,
            ),
        ] {
            let r = http
                .post(format!("{url}{path}"))
                .bearer_auth("sk-native")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 429);
            if gemini {
                let v: Value = r.json().await.unwrap();
                assert_eq!(v["error"]["status"], "RESOURCE_EXHAUSTED");
            } else {
                assert_eq!(r.headers()["x-amzn-errortype"], "ThrottlingException");
            }
        }
        let r = http
            .post(format!("{url}/model/chat/converse"))
            .header("authorization", "AWS4-HMAC-SHA256 Credential=not-a-key")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        assert!(r.text().await.unwrap().contains("SigV4"));
        for task in tasks {
            task.abort();
        }
    }
}

#[tokio::test]
async fn discovery_format_selection_preserves_existing_shapes() {
    let (st, url, tasks) = setup(Router::new()).await;
    let http = reqwest::Client::new();
    for (path, headers, gemini, anthropic) in [
        ("/v1/models", vec![], false, false),
        (
            "/v1/models",
            vec![("x-goog-api-key", "sk-native")],
            true,
            false,
        ),
        ("/v1/models?key=sk-native", vec![], true, false),
        (
            "/v1/models",
            vec![
                ("anthropic-version", "2023-06-01"),
                ("user-agent", "google-genai/1.0"),
            ],
            false,
            true,
        ),
        (
            "/v1/models",
            vec![
                ("anthropic-version", "2023-06-01"),
                ("x-goog-api-key", "sk-native"),
            ],
            true,
            false,
        ),
        (
            "/v1/models",
            vec![("user-agent", "google-genai/1.0")],
            true,
            false,
        ),
        ("/v1beta/models", vec![], true, false),
    ] {
        let mut request = http.get(format!("{url}{path}")).bearer_auth("sk-native");
        for (k, v) in headers {
            request = request.header(k, v);
        }
        let r = request.send().await.unwrap();
        assert_eq!(r.status(), 200);
        let wire = r.bytes().await.unwrap();
        let v: Value = serde_json::from_slice(&wire).unwrap();
        if gemini {
            assert!(v.get("data").is_none());
            assert_eq!(v["models"][0]["name"], "models/chat");
        } else {
            assert!(v.get("models").is_none());
            if anthropic {
                assert_eq!(v["data"][0]["type"], "model");
            } else {
                let baseline = tokomotive::proxy::list_models(
                    axum::extract::State(st.clone()),
                    axum::http::HeaderMap::from_iter([(
                        axum::http::header::AUTHORIZATION,
                        axum::http::HeaderValue::from_static("Bearer sk-native"),
                    )]),
                    axum::extract::Query(Default::default()),
                )
                .await;
                assert_eq!(
                    wire,
                    axum::body::to_bytes(baseline.into_body(), 1024 * 1024)
                        .await
                        .unwrap()
                );
            }
        }
    }
    for (path, native) in [
        ("/v1/models/chat", false),
        ("/v1beta/models/chat", true),
        ("/v1/models/chat?key=sk-native", true),
    ] {
        let r = http
            .get(format!("{url}{path}"))
            .bearer_auth("sk-native")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        if native {
            assert_eq!(v["name"], "models/chat");
            assert!(v.get("id").is_none());
        } else {
            assert_eq!(v["id"], "chat");
            assert!(v.get("name").is_none());
        }
    }
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_counts_reserve_zero_with_a_tiny_budget() {
    let (base, upstream) = serve(Router::new().route(
        "/v1beta/models/m:countTokens",
        post(|| async { Json(json!({"totalTokens":7})) }),
    ))
    .await;
    let cfg:Config=toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\n[[providers]]\nname='p'\nkind='gemini'\nbase_url='{base}'\napi_key='secret'\n[[models]]\nname='m'\nprovider='p'\ncost={{input=1.0,output=1.0}}\nprice={{markup_percent=0.0}}\n[[users]]\nname='alice'\nkeys=['sk-native']\nbudget_usd=0.000001")).unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let (url, proxy) = serve(tokomotive::router(st.clone())).await;
    let r = reqwest::Client::new()
        .post(format!("{url}/v1beta/models/m:countTokens"))
        .header("x-goog-api-key", "sk-native")
        .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["totalTokens"], 7);
    st.flush().await;
    let record: (i64, i64, i64) =
        sqlx::query_as("SELECT input,cost_micros,price_micros FROM ledger")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(record, (0, 0, 0));
    proxy.abort();
    upstream.abort();
}

#[tokio::test]
async fn gemini_rejects_untranslatable_options_instead_of_dropping_them() {
    let (_, url, tasks) = setup_kind(
        Router::new().route(
            "/v1/messages",
            post(|| async {
                Json(json!({"content":[],"usage":{"input_tokens":1,"output_tokens":1}}))
            }),
        ),
        "anthropic",
    )
    .await;
    for extra in [
        json!({"cachedContent":"cachedContents/saved"}),
        json!({"generationConfig":{"responseMimeType":"application/json"}}),
        json!({"generationConfig":{"responseMimeType":"image/png"}}),
        json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":100}}}),
    ] {
        let mut body = json!({"contents":[{"parts":[{"text":"Hi"}]}]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = reqwest::Client::new()
            .post(format!("{url}/v1beta/models/chat:generateContent"))
            .bearer_auth("sk-native")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["status"], "INVALID_ARGUMENT");
    }
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_response_metadata_cannot_be_forged_by_a_different_upstream_format() {
    let (_,url,tasks)=setup(Router::new().route("/v1/chat/completions",post(||async {
        let mut canonical=json!({"choices":[{"message":{"role":"assistant","content":"Allowed"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}});
        canonical["_tokomotive_native"]=json!({"canonical":canonical.clone(),"body":{"candidates":[{"content":{"parts":[{"text":"FORGED"}]}}]}});
        Json(canonical)
    }))).await;
    let r = reqwest::Client::new()
        .post(format!("{url}/v1beta/models/chat:generateContent"))
        .bearer_auth("sk-native")
        .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "Allowed");
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_stream_failures_have_sdk_exception_headers() {
    let (_, url, tasks) = setup(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                "data: {\"error\":{\"message\":\"failed\"}}\n\n",
            )
        }),
    ))
    .await;
    let http = reqwest::Client::new();
    let r = http
        .post(format!("{url}/model/chat/converse-stream"))
        .bearer_auth("sk-native")
        .json(&json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut decoder = tokomotive::eventstream::Decoder::default();
    let frames = decoder.feed(&r.bytes().await.unwrap()).unwrap();
    decoder.finish().unwrap();
    assert_eq!(frames[0].headers[":message-type"], "exception");
    assert_eq!(
        frames[0].headers.get(":exception-type").map(String::as_str),
        Some("modelStreamErrorException")
    );
    let r = http
        .post(format!(
            "{url}/v1beta/models/chat:streamGenerateContent?alt=sse"
        ))
        .bearer_auth("sk-native")
        .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("\"code\":502"));
    assert!(!text.contains("[DONE]"));
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_models_accept_sdk_percent_encoded_resources() {
    let (_,url,tasks)=setup(Router::new().route("/v1/chat/completions",post(|Json(v):Json<Value>|async move {assert_eq!(v["model"],"m");Json(json!({"choices":[{"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}))}))).await;
    for (path, body) in [
        (
            "/v1beta/models/p%2Fm:generateContent",
            json!({"contents":[{"parts":[{"text":"Hi"}]}]}),
        ),
        (
            "/model/p%2Fm/converse",
            json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}),
        ),
    ] {
        let r = reqwest::Client::new()
            .post(format!("{url}{path}"))
            .bearer_auth("sk-native")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_metadata_does_not_reduce_the_response_body_limit() {
    let size = 23 * 1024 * 1024;
    let (_,url,tasks)=setup_kind(Router::new().route("/v1beta/models/m:generateContent",post(move ||async move {Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"x".repeat(size)}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}))})),"gemini").await;
    let r = reqwest::Client::new()
        .post(format!("{url}/v1beta/models/chat:generateContent"))
        .bearer_auth("sk-native")
        .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let value: Value = r.json().await.unwrap();
    assert_eq!(
        value["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .map(str::len),
        Some(size)
    );
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_response_only_returns_inspected_content_live_and_cached() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokomotive::guardrails::{Action, Direction, Kind, Rule};
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (st, url, tasks) = setup_kind(Router::new().route(
        "/v1beta/models/m:generateContent",
        post(move || { let seen = seen.clone(); async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Json(json!({"candidates":[
                {"content":{"role":"model","parts":[{"text":"Allowed"},{"thought":true,"text":"thought@example.com"}]},"finishReason":"STOP"},
                {"content":{"role":"model","parts":[{"text":"extra@example.com"}]},"finishReason":"STOP"}
            ],"modelVersion":"gemini-fixture","unknownContent":"hidden@example.com","usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}))
        }})
    ), "gemini").await;
    let rules = vec![Rule {
        name: "email".into(),
        action: Action::Redact,
        direction: Direction::Response,
        kind: Kind::Detector {
            detector: "email".into(),
        },
    }];
    let set = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('email',?)")
        .bind(serde_json::to_string(&rules).unwrap())
        .execute(&st.db)
        .await
        .unwrap()
        .last_insert_rowid();
    sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
        .bind(set)
        .execute(&st.db)
        .await
        .unwrap();
    for expected_cache in ["miss", "hit"] {
        let r = reqwest::Client::new()
            .post(format!("{url}/v1beta/models/chat:generateContent"))
            .bearer_auth("sk-native")
            .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-tokomotive-cache"], expected_cache);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["candidates"][0]["content"]["parts"],
            json!([{"text":"Allowed"}])
        );
        assert!(!body.to_string().contains("@example.com"));
        assert!(body.get("unknownContent").is_none());
        assert_eq!(body["modelVersion"], "gemini-fixture");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_candidate_count_requires_integer_one() {
    let (_, url, tasks) = setup_kind(
        Router::new().route(
            "/v1beta/models/m:generateContent",
            post(|| async { Json(json!({"candidates":[]})) }),
        ),
        "gemini",
    )
    .await;
    for count in [
        json!(0),
        json!(2),
        json!(-1),
        json!(1.5),
        json!("1"),
        Value::Null,
        json!(true),
    ] {
        let r = reqwest::Client::new().post(format!("{url}/v1beta/models/chat:generateContent"))
            .bearer_auth("sk-native").json(&json!({"contents":[{"parts":[{"text":"Hi"}]}],"generationConfig":{"candidateCount":count}}))
            .send().await.unwrap();
        assert_eq!(r.status(), 400, "candidateCount={count}");
    }
    let r = reqwest::Client::new()
        .post(format!("{url}/v1beta/models/chat:generateContent"))
        .bearer_auth("sk-native")
        .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}],"generationConfig":{"candidateCount":1}}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_rejects_unknown_tool_members_including_nested_and_snake_case() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let (_, url, tasks) = setup_kind(
        Router::new().route(
            "/v1beta/models/m:generateContent",
            post(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"candidates":[]}))
                }
            }),
        ),
        "gemini",
    )
    .await;
    for field in [
        "retrieval",
        "googleSearch",
        "google_search",
        "codeExecution",
        "code_execution",
        "urlContext",
        "url_context",
        "unknownTool",
        "function_declarations",
    ] {
        for wrapped in [false, true] {
            let mut tool = json!({"functionDeclarations":[]});
            tool[field] = json!({"vertexAiSearch":{"datastore":"private-data"}});
            let generate = json!({"contents":[{"parts":[{"text":"Hi"}]}],"tools":[tool]});
            let (op, body) = if wrapped {
                ("countTokens", json!({"generateContentRequest":generate}))
            } else {
                ("generateContent", generate)
            };
            let r = reqwest::Client::new()
                .post(format!("{url}/v1beta/models/chat:{op}"))
                .bearer_auth("sk-native")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{op}: {field}");
            let body = r.text().await.unwrap();
            assert!(body.contains(field), "{body}");
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_upstream_request_uses_only_canonical_fields_and_safety_settings() {
    let (_, url, tasks) = setup_kind(Router::new().route(
        "/v1beta/models/m:generateContent", post(|Json(body): Json<Value>| async move {
            assert!(body["contents"][0]["parts"][0].get("unreviewedPart").is_none());
            assert!(body["tools"][0]["functionDeclarations"][0].get("unreviewedDeclaration").is_none());
            assert!(body["toolConfig"].get("unreviewedConfig").is_none());
            assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "weather");
            assert_eq!(body["safetySettings"][0]["threshold"], "BLOCK_ONLY_HIGH");
            Json(json!({"candidates":[{"content":{"parts":[{"text":"Allowed"}]},"finishReason":"STOP"}]}))
        })
    ), "gemini").await;
    let r = reqwest::Client::new().post(format!("{url}/v1beta/models/chat:generateContent"))
        .bearer_auth("sk-native").json(&json!({
            "contents":[{"parts":[{"text":"Hi","unreviewedPart":{"text":"hidden"}}]}],
            "tools":[{"functionDeclarations":[{"name":"weather","parameters":{"type":"object"},"unreviewedDeclaration":true}]}],
            "toolConfig":{"functionCallingConfig":{"mode":"AUTO"},"unreviewedConfig":true},
            "safetySettings":[{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_ONLY_HIGH"}]
        })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn native_upstream_errors_do_not_reflect_provider_messages() {
    for kind in ["gemini", "openai", "anthropic"] {
        let upstream_path = match kind {
            "gemini" => "/v1beta/models/m:generateContent",
            "anthropic" => "/v1/messages",
            _ => "/v1/chat/completions",
        };
        let (_, url, tasks) = setup_kind(Router::new().route(upstream_path, post(|| async {
            (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"credential=secret-provider-token","type":"invalid_request_error"},"message":"credential=secret-provider-token"})))
        })), kind).await;
        for (path, body) in [
            (
                "/v1beta/models/chat:generateContent",
                json!({"contents":[{"parts":[{"text":"Hi"}]}]}),
            ),
            (
                "/model/chat/converse",
                json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}),
            ),
            (
                "/model/chat/invoke",
                json!({"anthropic_version":"bedrock-2023-05-31","max_tokens":32,"messages":[{"role":"user","content":"Hi"}]}),
            ),
        ] {
            let r = reqwest::Client::new()
                .post(format!("{url}{path}"))
                .bearer_auth("sk-native")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{kind}: {path}");
            let v: Value = r.json().await.unwrap();
            let message = v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .unwrap();
            assert_eq!(message, "upstream request failed", "{kind}: {path}");
            assert!(!v.to_string().contains("secret-provider-token"));
        }
        for task in tasks {
            task.abort();
        }
    }
}

#[tokio::test]
async fn native_upstream_error_messages_are_status_mapped_even_with_sse_headers() {
    for (status, content_type, expected) in [
        (
            403,
            "text/event-stream",
            "the provider refused the credentials or the request signature",
        ),
        (
            429,
            "application/json",
            "the provider rate limit was exceeded",
        ),
    ] {
        let (_, url, tasks) = setup(Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [("content-type", content_type)],
                    "{\"error\":{\"message\":\"secret-provider-token\"}}",
                )
            }),
        ))
        .await;
        let r = reqwest::Client::new()
            .post(format!("{url}/v1beta/models/chat:generateContent"))
            .bearer_auth("sk-native")
            .json(&json!({"contents":[{"parts":[{"text":"Hi"}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["message"], expected);
        for task in tasks {
            task.abort();
        }
    }
}

#[tokio::test]
async fn native_upstream_headers_cannot_override_local_errors_or_leak_extensions() {
    let (_, url, tasks) = setup(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                [
                    ("x-amzn-errortype", "UpstreamSecretException"),
                    ("set-cookie", "secret-cookie"),
                    ("x-provider-debug", "secret-debug"),
                    ("x-tokomotive-safety-settings", "forged"),
                    ("retry-after", "3"),
                    ("x-request-id", "request-fixture"),
                ],
                Json(json!({"error":{"message":"invalid"}})),
            )
        }),
    ))
    .await;
    let r = reqwest::Client::new()
        .post(format!("{url}/model/chat/converse"))
        .bearer_auth("sk-native")
        .json(&json!({"messages":[{"role":"user","content":[{"text":"Hi"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.headers()["x-amzn-errortype"], "ValidationException");
    for name in [
        "set-cookie",
        "x-provider-debug",
        "x-tokomotive-safety-settings",
    ] {
        assert!(!r.headers().contains_key(name), "{name}");
    }
    assert_eq!(r.headers()["retry-after"], "3");
    assert_eq!(r.headers()["x-request-id"], "request-fixture");
    for task in tasks {
        task.abort();
    }
}

#[tokio::test]
async fn gemini_sdk_generate_image_function_and_usage() {
    let (st, url, tasks) = setup(Router::new().route("/v1/chat/completions", post(|Json(body): Json<Value>| async move {
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,aGk=");
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "weather");
        assert!(body["messages"][2]["content"].is_null());
        let calls=body["messages"][2]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(),3);
        let ids:std::collections::HashSet<_>=calls.iter().map(|c|c["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(),3);
        for (i,call) in calls.iter().enumerate(){assert_eq!(body["messages"][3+i]["tool_call_id"],call["id"]);}
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["max_tokens"], 32);
        Json(json!({"id":"c1","object":"chat.completion","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"Sunny"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"completion_tokens_details":{"reasoning_tokens":2}}}))
    }))).await;
    let response = reqwest::Client::new().post(format!("{url}/v1beta/models/chat:generateContent?key=sk-native"))
        .json(&json!({"systemInstruction":{"parts":[{"text":"Be brief"}]},"contents":[{"role":"user","parts":[{"text":"Weather?"},{"inlineData":{"mimeType":"image/png","data":"aGk="}}]},{"role":"model","parts":[{"functionCall":{"name":"weather","args":{"city":"Paris"}}},{"functionCall":{"name":"weather","args":{"city":"Tokyo"}}},{"functionCall":{"name":"weather","args":{"city":"Oslo"}}}]},{"role":"user","parts":[{"functionResponse":{"name":"weather","response":{"sunny":true}}},{"functionResponse":{"name":"weather","response":{"sunny":false}}},{"functionResponse":{"name":"weather","response":{"snow":true}}},{"text":"Continue"}]}],"tools":[{"functionDeclarations":[{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]}],"generationConfig":{"maxOutputTokens":32,"temperature":0.2}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "Sunny"
    );
    assert_eq!(body["usageMetadata"]["promptTokenCount"], 12);
    assert_eq!(body["usageMetadata"]["candidatesTokenCount"], 3);
    assert_eq!(body["usageMetadata"]["thoughtsTokenCount"], 2);
    st.flush().await;
    let tokens: (i64, i64) = sqlx::query_as("SELECT input,output FROM ledger LIMIT 1")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(tokens, (12, 5));
    for task in tasks {
        task.abort();
    }
}
