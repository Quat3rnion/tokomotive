use axum::{Json, extract::State, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokomotive::{config::Config, state::AppState};

struct Env {
    st: AppState,
    url: String,
    calls: Arc<AtomicUsize>,
    stream_gate: Arc<tokio::sync::Notify>,
    embed_inputs: Arc<std::sync::Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    stream_gate: Arc<tokio::sync::Notify>,
    embed_inputs: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn answer(
    State(mock): State<Mock>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    mock.calls.fetch_add(1, Ordering::SeqCst);
    if body["messages"][0]["content"] == "error" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"bad request"}})),
        )
            .into_response();
    }
    if uri.path().ends_with("responses") {
        if body["stream"] == true {
            return ([("content-type","text/event-stream")],"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"m\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n").into_response();
        }
        return Json(json!({"id":"r","object":"response","status":if body["input"] == "incomplete" { "incomplete" } else { "completed" },"model":"m","output":[],"usage":{"input_tokens":2,"output_tokens":3}})).into_response();
    }
    if uri.path().ends_with("embeddings") {
        let input = body["input"].as_str().unwrap_or_default();
        mock.embed_inputs.lock().unwrap().push(input.into());
        if input == "embed-error" {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":"embedding failed"}})),
            )
                .into_response();
        }
        if input == "embed-slow" {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        if input == "embed-gate" {
            mock.stream_gate.notified().await;
        }
        let vector = if input == "near" {
            vec![0.96, 0.28]
        } else if input == "unrelated" {
            vec![0.0, 1.0]
        } else {
            vec![1.0, 0.0]
        };
        return Json(json!({"object":"list","model":body["model"],"data":[{"object":"embedding","index":0,"embedding":vector}],"usage":{"prompt_tokens":2,"total_tokens":2}})).into_response();
    }
    if body["stream"] == true {
        let s = if uri.path().ends_with("messages") {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        } else {
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"c\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n"
        };
        let s = if body["messages"][0]["content"] == "partial" {
            s.replace("data: [DONE]\n\n", "").replace(
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                "",
            )
        } else {
            s.to_owned()
        };
        if body["messages"][0]["content"] == "abort" {
            use futures_util::StreamExt;
            let initial = s.replace("data: [DONE]\n\n", "");
            let parts =
                futures_util::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(initial))])
                    .chain(futures_util::stream::once(async move {
                        mock.stream_gate.notified().await;
                        Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n"))
                    }));
            let mut r = axum::response::Response::new(axum::body::Body::from_stream(parts));
            r.headers_mut().insert(
                "content-type",
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            return r;
        }
        return ([("content-type", "text/event-stream")], s).into_response();
    }
    Json(json!({"id":"c","model":"m-version","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}})).into_response()
}
async fn env(scope: &str, kind: &str) -> Env {
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let stream_gate = Arc::new(tokio::sync::Notify::new());
    let embed_inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(answer))
        .route("/v1/messages", post(answer))
        .route("/v1/responses", post(answer))
        .route("/v1/embeddings", post(answer))
        .with_state(Mock {
            calls: calls.clone(),
            stream_gate: stream_gate.clone(),
            embed_inputs: embed_inputs.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let mut tasks = vec![tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    })];
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = toml::from_str(&format!("[server]\ndatabase='sqlite://{}'\nadmin_password='pw'\n[[providers]]\nname='p'\nkind='{kind}'\nbase_url='http://{upstream}'\napi_key='secret'\n[[routes]]\nname='cached'\ndeployments=[{{provider='p',model='m'}}]\ncache={{enabled=true,ttl_secs=60,scope='{scope}'}}\n[[users]]\nname='alice'\nkeys=['sk-alice']\n[[users]]\nname='bob'\nkeys=['sk-bob']\n", dir.path().join("test.db").display())).unwrap();
    cfg.validate().unwrap();
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
    let app = tokomotive::router(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    Env {
        st,
        url,
        calls,
        stream_gate,
        embed_inputs,
        tasks,
        _dir: dir,
    }
}
impl Env {
    async fn send(
        &self,
        key: &str,
        endpoint: &str,
        body: Value,
        control: Option<(&str, &str)>,
    ) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(format!("{}/v1/{endpoint}", self.url))
            .bearer_auth(key)
            .json(&body);
        if let Some((name, value)) = control {
            req = req.header(name, value);
        }
        req.send().await.unwrap()
    }
}

#[tokio::test]
async fn resolution_headers_cover_json_streams_and_cache_hits() {
    let e = env("key", "openai").await;
    for stream in [false, true] {
        for expected in ["miss", "hit"] {
            let r=e.send("sk-alice","chat/completions",json!({"model":"cached","messages":[{"role":"user","content":"hi"}],"stream":stream}),None).await;
            assert_eq!(r.status(), 200);
            assert_eq!(r.headers()["x-tokomotive-cache"], expected);
            assert_eq!(r.headers()["x-tokomotive-route"], "cached");
            assert_eq!(r.headers()["x-tokomotive-provider"], "p");
            assert_eq!(r.headers()["x-tokomotive-model"], "m");
            r.bytes().await.unwrap();
            e.st.flush().await;
        }
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
}
fn body() -> Value {
    json!({"model":"cached","messages":[{"role":"user","content":"hello"}]})
}

async fn semantic_env(scope: &str) -> Env {
    let e = env(scope, "openai").await;
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic',json(?))")
        .bind(json!({"enabled":true,"embedding_model":"p/m","scope":scope}).to_string())
        .execute(&e.st.db)
        .await
        .unwrap();
    e.st.reload_settings().await.unwrap();
    e
}
fn semantic_body(text: &str) -> Value {
    json!({"model":"cached","messages":[{"role":"user","content":text}]})
}

#[tokio::test]
async fn semantic_hit_above_threshold_miss_below_and_exact_first() {
    let e = semantic_env("key").await;
    for (text, expected) in [
        ("hello", "miss"),
        ("hello", "hit"),
        ("similar", "semantic-hit; similarity=1.000"),
        ("near", "semantic-hit; similarity=0.960"),
        ("unrelated", "miss"),
    ] {
        let r = e
            .send("sk-alice", "chat/completions", semantic_body(text), None)
            .await;
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()["x-tokomotive-cache"],
            expected,
            "text={text}; calls={}; rows={:?}",
            e.calls.load(Ordering::SeqCst),
            sqlx::query_as::<_, (String, String, i64)>(
                "SELECT endpoint,requested_model,status FROM ledger"
            )
            .fetch_all(&e.st.db)
            .await
            .unwrap()
        );
        r.bytes().await.unwrap();
        e.st.flush().await;
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 6); // Four embeddings, two chats.
}

#[tokio::test]
async fn semantic_ledger_flags_price_and_metrics_are_distinct() {
    let e = semantic_env("key").await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.cache.hit_price_micros',7) WHERE key='runtime_settings'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    for text in ["hello", "similar"] {
        e.send("sk-alice", "chat/completions", semantic_body(text), None)
            .await
            .text()
            .await
            .unwrap();
        e.st.flush().await;
    }
    let flags:Vec<(String,i64,i64,i64,i64,i64)> = sqlx::query_as("SELECT endpoint,semantic_cache_embed,semantic_cache_hit,cache_hit,cost_micros,price_micros FROM ledger ORDER BY id")
        .fetch_all(&e.st.db).await.unwrap();
    assert_eq!(flags.len(), 4);
    assert_eq!(
        (&flags[0].0, flags[0].1, flags[0].2),
        (&"embeddings".to_owned(), 1, 0)
    );
    assert_eq!(flags[2].1, 1);
    assert_eq!(flags[3], ("chat_completions".into(), 0, 1, 1, 0, 7));
    tokomotive::db::create_session(&e.st.db, "semantic-ui", 1)
        .await
        .unwrap();
    let ledger = reqwest::Client::new()
        .get(format!("{}/admin/ledger", e.url))
        .header("cookie", "tokomotive_session=semantic-ui")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(ledger.contains("semantic_cache_hit"));
    assert!(ledger.contains("semantic_cache_embed"));
    let metrics = reqwest::get(format!("{}/metrics", e.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("llm_semantic_cache_hits_total"));
    assert!(metrics.contains("cache_kind=\"semantic\""));
    assert!(metrics.contains("semantic_cache_embed=\"true\""));
}

#[tokio::test]
async fn semantic_scope_isolates_keys_and_teams_but_global_shares() {
    for (scope, shared) in [("key", false), ("team", true), ("global", true)] {
        let e = semantic_env(scope).await;
        if scope == "team" {
            let a = tokomotive::db::create_team(&e.st.db, "a").await.unwrap();
            let b = tokomotive::db::create_team(&e.st.db, "b").await.unwrap();
            sqlx::query("UPDATE users SET team_id=?")
                .bind(a)
                .execute(&e.st.db)
                .await
                .unwrap();
            // a second Team must never reuse the first Team's answer.
            e.send("sk-alice", "chat/completions", semantic_body("hello"), None)
                .await
                .text()
                .await
                .unwrap();
            sqlx::query("UPDATE users SET team_id=? WHERE name='bob'")
                .bind(b)
                .execute(&e.st.db)
                .await
                .unwrap();
            let r = e
                .send(
                    "sk-bob",
                    "chat/completions",
                    semantic_body("different team"),
                    None,
                )
                .await;
            assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
            r.bytes().await.unwrap();
            sqlx::query("UPDATE users SET team_id=? WHERE name='bob'")
                .bind(a)
                .execute(&e.st.db)
                .await
                .unwrap();
        } else {
            e.send("sk-alice", "chat/completions", semantic_body("hello"), None)
                .await
                .text()
                .await
                .unwrap();
        }
        let r = e
            .send("sk-bob", "chat/completions", semantic_body("similar"), None)
            .await;
        assert_eq!(
            r.headers()["x-tokomotive-cache"]
                .to_str()
                .unwrap()
                .starts_with("semantic-hit"),
            shared,
            "{scope}"
        );
        r.bytes().await.unwrap();
        e.st.flush().await;
    }
    let e = semantic_env("team").await;
    for text in ["hello", "similar"] {
        let r = e
            .send("sk-alice", "chat/completions", semantic_body(text), None)
            .await;
        // No Team means no scope, not a shared anonymous Team.
        assert_eq!(r.headers()["x-tokomotive-cache"], "bypass");
        r.bytes().await.unwrap();
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
    e.st.flush().await;
}

#[tokio::test]
async fn semantic_parameters_tools_system_and_stream_are_bucket_boundaries() {
    let e = semantic_env("key").await;
    e.send("sk-alice", "chat/completions", semantic_body("hello"), None)
        .await
        .text()
        .await
        .unwrap();
    for (field, value) in [
        ("temperature", json!(0.1)),
        ("max_tokens", json!(12)),
        ("top_p", json!(0.5)),
        ("response_format", json!({"type":"json_object"})),
        (
            "tools",
            json!([{"type":"function","function":{"name":"one"}}]),
        ),
        (
            "tools",
            json!([{"type":"function","function":{"name":"two"}}]),
        ),
        ("future_provider_option", json!(true)),
        ("semantic_system_hash", json!("client option")),
        ("stream", json!(true)),
    ] {
        let mut body = semantic_body("similar");
        body[field] = value;
        let r = e
            .send("sk-alice", "chat/completions", body.clone(), None)
            .await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss", "{field}");
        r.bytes().await.unwrap();
        body["messages"][0]["content"] = json!("another similar request");
        let r = e.send("sk-alice", "chat/completions", body, None).await;
        assert!(
            r.headers()["x-tokomotive-cache"]
                .to_str()
                .unwrap()
                .starts_with("semantic-hit"),
            "{field}"
        );
        r.bytes().await.unwrap();
    }
    for system in ["first system", "changed system"] {
        let r=e.send("sk-alice","chat/completions",json!({"model":"cached","messages":[{"role":"system","content":system},{"role":"user","content":"hello"}]}),None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.bytes().await.unwrap();
    }
    for control in [
        ("cache-control", "no-store"),
        ("x-tokomotive-cache", "bypass"),
    ] {
        let r = e
            .send(
                "sk-alice",
                "chat/completions",
                semantic_body("new"),
                Some(control),
            )
            .await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "bypass");
        r.bytes().await.unwrap();
    }
    let mut n = semantic_body("new");
    n["n"] = json!(2);
    let r = e.send("sk-alice", "chat/completions", n, None).await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "bypass");
    r.bytes().await.unwrap();
    e.st.flush().await;
}

#[tokio::test]
async fn semantic_stream_replay_and_embedding_failure_timeout_fallback() {
    let e = semantic_env("key").await;
    let mut first = semantic_body("hello");
    first["stream"] = json!(true);
    let r = e
        .send("sk-alice", "chat/completions", first.clone(), None)
        .await;
    let original = r.text().await.unwrap();
    e.st.flush().await;
    first["messages"][0]["content"] = json!("similar");
    let r = e.send("sk-alice", "chat/completions", first, None).await;
    assert!(
        r.headers()["x-tokomotive-cache"]
            .to_str()
            .unwrap()
            .starts_with("semantic-hit")
    );
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    assert_eq!(r.text().await.unwrap(), original);
    e.st.flush().await;
    sqlx::query(
        "UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic.timeout_ms',20)",
    )
    .execute(&e.st.db)
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    for text in ["embed-error", "embed-slow"] {
        let r = e
            .send("sk-alice", "chat/completions", semantic_body(text), None)
            .await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.bytes().await.unwrap();
        e.st.flush().await;
    }
    let flags:Vec<(i64,i64)> = sqlx::query_as("SELECT status,semantic_cache_embed FROM ledger WHERE endpoint='embeddings' ORDER BY id DESC LIMIT 2").fetch_all(&e.st.db).await.unwrap();
    assert_eq!(flags, vec![(200, 1), (400, 1)]);
    let vectors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_cache WHERE semantic_bucket IS NOT NULL")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert_eq!(vectors, 1); // Neither the failed nor timed-out embedding is stored.
}

#[tokio::test]
async fn semantic_input_modes_limits_and_message_metadata_are_safe() {
    let e = semantic_env("key").await;
    sqlx::query(
        "UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic.max_chars',4)",
    )
    .execute(&e.st.db)
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    let input = json!({"model":"cached","messages":[{"role":"user","content":"old"},{"role":"assistant","content":"answer"},{"role":"user","content":"hello"}]});
    e.send("sk-alice", "chat/completions", input.clone(), None)
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(e.embed_inputs.lock().unwrap().last().unwrap(), "hell");
    let mut named = input.clone();
    named["messages"][2]["name"] = json!("another_person");
    let r = e.send("sk-alice", "chat/completions", named, None).await;
    assert!(
        !r.headers()["x-tokomotive-cache"]
            .to_str()
            .unwrap()
            .starts_with("semantic-hit")
    );
    r.bytes().await.unwrap();
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic.input','conversation','$.cache.semantic.max_chars',1000)").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    e.send("sk-alice", "chat/completions", input, None)
        .await
        .bytes()
        .await
        .unwrap();
    let text = e.embed_inputs.lock().unwrap().last().unwrap().clone();
    assert!(text.contains("answer"));
    assert!(text.contains("old"));
    assert!(text.contains("hello"));
    e.st.flush().await;
}

#[tokio::test]
async fn semantic_storage_encrypts_vectors_counts_bytes_expires_and_bounds_search() {
    use tokomotive::{cache, semantic_cache as semantic};
    let e = env("key", "openai").await;
    let policy = semantic::Policy {
        enabled: true,
        embedding_model: "p/m".into(),
        max_vectors: 3,
        ..Default::default()
    };
    let plan = |text: &str| {
        semantic::Plan::new(
            &policy,
            "route",
            "/v1/chat/completions",
            &semantic_body(text),
            &Default::default(),
            1,
            None,
        )
        .unwrap()
    };
    let mut entry = cache::Entry {
        body: String::new(),
        content_type: "application/json".into(),
        provider: "p".into(),
        upstream_model: Some("m".into()),
        model: Some("m".into()),
        usage: Default::default(),
        estimated: false,
    };
    entry.set_body(b"private answer");
    let limits = cache::Limits::default();
    for (text, vector) in [
        ("old", vec![1., 0.]),
        ("newer", vec![0., 1.]),
        ("newest", vec![0., 1.]),
    ] {
        semantic::store(
            &e.st.db,
            &e.st.secret_key,
            &semantic::WritePlan {
                plan: plan(text),
                vector,
            },
            &limits,
            &entry,
        )
        .await
        .unwrap();
    }
    let (body, vector, bytes): (String, Vec<u8>, i64) =
        sqlx::query_as("SELECT body,vector,body_bytes FROM response_cache WHERE key=?")
            .bind(plan("old").exact.key)
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert!(!body.contains("private answer"));
    assert_ne!(vector, [1_f32.to_le_bytes(), 0_f32.to_le_bytes()].concat());
    assert_eq!(bytes as usize, body.len() + vector.len());
    let decoded: (String, String, String) = serde_json::from_str(
        &e.st
            .secret_key
            .decrypt(std::str::from_utf8(&vector).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.2, "AACAPwAAAAA="); // f32 little-endian [1, 0], inside encryption.
    let full = semantic::lookup(
        &e.st.db,
        &e.st.secret_key,
        &plan("query"),
        &[1., 0.],
        &limits,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(full.0.bytes().unwrap(), b"private answer");
    assert_eq!(full.1, 1.0);
    let mut bounded = plan("query");
    bounded.policy.max_vectors = 2;
    assert!(
        semantic::lookup(&e.st.db, &e.st.secret_key, &bounded, &[1., 0.], &limits)
            .await
            .unwrap()
            .is_none()
    );
    // Storage also enforces M; newest vectors survive when the bucket fills.
    semantic::store(
        &e.st.db,
        &e.st.secret_key,
        &semantic::WritePlan {
            plan: plan("fourth"),
            vector: vec![0., 1.],
        },
        &limits,
        &entry,
    )
    .await
    .unwrap();
    assert!(
        semantic::lookup(
            &e.st.db,
            &e.st.secret_key,
            &plan("query"),
            &[1., 0.],
            &limits
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_cache")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        3
    );
    let tiny = cache::Limits {
        max_entry_bytes: body.len() as u64,
        ..limits.clone()
    };
    semantic::store(
        &e.st.db,
        &e.st.secret_key,
        &semantic::WritePlan {
            plan: plan("too-large"),
            vector: vec![0., 1.],
        },
        &tiny,
        &entry,
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_cache WHERE key=?")
            .bind(plan("too-large").exact.key)
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        0
    );
    // Corrupt/transplanted vectors are ignored, not returned as a match.
    sqlx::query("UPDATE response_cache SET vector=?")
        .bind(vector)
        .execute(&e.st.db)
        .await
        .unwrap();
    assert!(
        semantic::lookup(
            &e.st.db,
            &e.st.secret_key,
            &plan("query"),
            &[1., 0.],
            &limits
        )
        .await
        .unwrap()
        .is_none()
    );
    sqlx::query("UPDATE response_cache SET expires_ms=0")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert!(
        semantic::lookup(
            &e.st.db,
            &e.st.secret_key,
            &plan("query"),
            &[0., 1.],
            &limits
        )
        .await
        .unwrap()
        .is_none()
    );
    cache::purge(&e.st.db).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_cache")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        0
    );
    semantic::store(
        &e.st.db,
        &e.st.secret_key,
        &semantic::WritePlan {
            plan: plan("byte-budget"),
            vector: vec![0., 1.],
        },
        &limits,
        &entry,
    )
    .await
    .unwrap();
    let size: i64 = sqlx::query_scalar("SELECT SUM(body_bytes) FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    cache::maintain(
        &e.st.db,
        &cache::Limits {
            max_bytes: (size - 1) as u64,
            ..limits
        },
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_cache")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        0
    );
    e.st.flush().await;
}

#[tokio::test]
async fn semantic_only_cache_does_not_store_partial_streams() {
    let e = semantic_env("key").await;
    sqlx::query(
        "UPDATE routes SET config_json=json_set(config_json,'$.cache.enabled',json('false'))",
    )
    .execute(&e.st.db)
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    for (text, expected) in [
        ("partial", "miss"),
        ("hello", "miss"),
        ("similar", "semantic-hit; similarity=1.000"),
    ] {
        let mut body = semantic_body(text);
        body["stream"] = json!(true);
        let r = e.send("sk-alice", "chat/completions", body, None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], expected);
        r.bytes().await.unwrap();
        e.st.flush().await;
    }
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT semantic_bucket,COUNT(*) FROM response_cache GROUP BY semantic_bucket",
    )
    .fetch_all(&e.st.db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].0.is_some());
    assert_eq!(rows[0].1, 1);
}

#[tokio::test]
async fn semantic_configuration_reload_during_embedding_skips_storage() {
    let e = semantic_env("key").await;
    let url = e.url.clone();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("sk-alice")
            .json(&semantic_body("embed-gate"))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while e.embed_inputs.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    e.stream_gate.notify_one();
    assert_eq!(request.await.unwrap().status(), 200);
    e.st.flush().await;
    let vectors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_cache WHERE semantic_bucket IS NOT NULL")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert_eq!(vectors, 0);
}

#[tokio::test]
async fn semantic_native_formats_and_embedding_deployments_do_not_share() {
    let e = semantic_env("key").await;
    for (text, expected) in [
        ("native hello", "miss"),
        ("native similar", "semantic-hit; similarity=1.000"),
    ] {
        let r = reqwest::Client::new()
            .post(format!(
                "{}/v1beta/models/cached:generateContent?key=sk-alice",
                e.url
            ))
            .json(&json!({"contents":[{"role":"user","parts":[{"text":text}]}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-tokomotive-cache"], expected);
        assert!(r.json::<Value>().await.unwrap()["candidates"].is_array());
        e.st.flush().await;
    }
    let r = e
        .send(
            "sk-alice",
            "chat/completions",
            semantic_body("openai similar"),
            None,
        )
        .await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    r.bytes().await.unwrap();
    sqlx::query("INSERT INTO routes(name,config_json) VALUES('embed-route',?)").bind(json!({"name":"embed-route","deployments":[{"provider":"p","model":"embed-one","weight":1}]}).to_string()).execute(&e.st.db).await.unwrap();
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic.embedding_model','embed-route') WHERE name='cached'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    e.send(
        "sk-alice",
        "chat/completions",
        semantic_body("first space"),
        None,
    )
    .await
    .bytes()
    .await
    .unwrap();
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.deployments[0].model','embed-two') WHERE name='embed-route'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    let r = e
        .send(
            "sk-alice",
            "chat/completions",
            semantic_body("new space"),
            None,
        )
        .await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    r.bytes().await.unwrap();
    e.st.flush().await;
}

#[tokio::test]
async fn semantic_embeddings_use_resolver_ledger_budgets_and_rate_limits() {
    use tokomotive::{
        db,
        pricing::{CardRates, PriceSpec, Rates},
    };
    let e = semantic_env("global").await;
    let model = db::create_model(&e.st.db, "embed", "p", &["embed-alias".into()])
        .await
        .unwrap();
    db::add_cost_card(
        &e.st.db,
        model,
        &CardRates {
            base: Rates {
                input: 500_000,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db::add_price_card(
        &e.st.db,
        model,
        None,
        &PriceSpec::Rates(CardRates {
            base: Rates {
                input: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic.embedding_model','embed-alias')").execute(&e.st.db).await.unwrap();
    sqlx::query("UPDATE settings SET value=json_set(value,'$.cache.hit_price_micros',7) WHERE key='runtime_settings'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    e.send("sk-alice", "chat/completions", semantic_body("warm"), None)
        .await
        .bytes()
        .await
        .unwrap();
    e.st.flush().await;
    sqlx::query("UPDATE users SET budget_micros=1,tpm_limit=6 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e
        .send(
            "sk-bob",
            "chat/completions",
            semantic_body("budget fallback"),
            None,
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert_eq!(e.embed_inputs.lock().unwrap().len(), 1); // Budget denied the second embedding before send.
    sqlx::query("UPDATE users SET budget_micros=100,tpm_limit=6 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e
        .send("sk-bob", "chat/completions", semantic_body("match"), None)
        .await;
    assert!(
        r.headers()["x-tokomotive-cache"]
            .to_str()
            .unwrap()
            .starts_with("semantic-hit")
    );
    r.bytes().await.unwrap();
    e.st.flush().await;
    let rows:Vec<(i64,i64,i64,i64)> = sqlx::query_as("SELECT semantic_cache_embed,semantic_cache_hit,cost_micros,price_micros FROM ledger WHERE user_id=(SELECT id FROM users WHERE name='bob') ORDER BY id").fetch_all(&e.st.db).await.unwrap();
    assert_eq!(rows, vec![(0, 0, 0, 0), (1, 0, 1, 2), (0, 1, 0, 7)]);
    let bob: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name='bob'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(db::spend_since(&e.st.db, bob, 0).await.unwrap(), 9);
    assert_eq!(
        e.send("sk-bob", "chat/completions", semantic_body("limited"), None)
            .await
            .status(),
        429
    );
    sqlx::query("UPDATE users SET budget_micros=50,tpm_limit=NULL WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE settings SET value=json_set(value,'$.cache.hit_price_micros',100) WHERE key='runtime_settings'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    assert_eq!(
        e.send(
            "sk-bob",
            "chat/completions",
            semantic_body("hit price exceeds budget"),
            None
        )
        .await
        .status(),
        402
    );
    e.st.flush().await;
    // The embedding was permitted and billed, but the hit Price was not.
    assert_eq!(db::spend_since(&e.st.db, bob, 0).await.unwrap(), 11);
    let e = semantic_env("key").await;
    sqlx::query("UPDATE users SET rpm_limit=1 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e
        .send("sk-bob", "chat/completions", semantic_body("hello"), None)
        .await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert!(e.embed_inputs.lock().unwrap().is_empty()); // Outer admission used the one RPM slot.
    assert_eq!(
        e.send("sk-bob", "chat/completions", semantic_body("again"), None)
            .await
            .status(),
        429
    );
}

#[tokio::test]
async fn semantic_hits_apply_new_response_guardrails_and_request_rules_run_first() {
    let e = semantic_env("key").await;
    for stream in [false, true] {
        let mut body = semantic_body("warm");
        body["stream"] = json!(stream);
        e.send("sk-alice", "chat/completions", body, None)
            .await
            .bytes()
            .await
            .unwrap();
        e.st.flush().await;
    }
    let rules = r#"[{"name":"mask","kind":{"type":"keywords","words":["ok"]},"action":"redact","direction":"response"}]"#;
    let set = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('semantic-mask',?)")
        .bind(rules)
        .execute(&e.st.db)
        .await
        .unwrap()
        .last_insert_rowid();
    sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'cached')")
        .bind(set)
        .execute(&e.st.db)
        .await
        .unwrap();
    for stream in [false, true] {
        let mut body = semantic_body("similar");
        body["stream"] = json!(stream);
        let r = e.send("sk-alice", "chat/completions", body, None).await;
        assert!(
            r.headers()["x-tokomotive-cache"]
                .to_str()
                .unwrap()
                .starts_with("semantic-hit")
        );
        assert!(r.text().await.unwrap().contains("[REDACTED:mask]"));
        e.st.flush().await;
    }
    let calls = e.calls.load(Ordering::SeqCst);
    sqlx::query("UPDATE guardrail_sets SET rules=? WHERE id=?").bind(r#"[{"name":"deny","kind":{"type":"keywords","words":["blocked"]},"action":"block","direction":"request"}]"#).bind(set).execute(&e.st.db).await.unwrap();
    assert_eq!(
        e.send(
            "sk-alice",
            "chat/completions",
            semantic_body("blocked"),
            None
        )
        .await
        .status(),
        400
    );
    assert_eq!(e.calls.load(Ordering::SeqCst), calls);
    e.st.flush().await;
    let hits:i64=sqlx::query_scalar("SELECT COUNT(*) FROM guardrail_events g JOIN ledger l ON l.id=g.ledger_id WHERE l.semantic_cache_hit=1").fetch_one(&e.st.db).await.unwrap();
    assert_eq!(hits, 2);
}

#[tokio::test]
async fn semantic_rule_ids_and_canary_sides_are_isolated() {
    use tokomotive::route_rules::{self, Action, Canary, Rule};
    let e = semantic_env("key").await;
    let action = Action::Deployments {
        deployments: vec![("p".into(), "m".into())],
    };
    let mut rules = vec![Rule {
        id: "rule-one".into(),
        name: "canary".into(),
        enabled: true,
        conditions: Default::default(),
        no_fallback: false,
        canary: Some(Canary {
            percent_bp: 5000,
            salt: "stable".into(),
            action: action.clone(),
        }),
        action,
    }];
    sqlx::query("UPDATE routes SET rules=? WHERE name='cached'")
        .bind(serde_json::to_string(&rules).unwrap())
        .execute(&e.st.db)
        .await
        .unwrap();
    let mut sessions = std::collections::BTreeMap::new();
    for i in 0..100 {
        let session = format!("session-{i}");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-session-id", session.parse().unwrap());
        let side = route_rules::select(
            &rules,
            &route_rules::Attributes {
                key: 1,
                user: 1,
                team: None,
                headers: &headers,
                body: None,
                model: "cached",
                endpoint: "chat_completions",
                format: "openai",
            },
        )
        .unwrap()
        .side;
        sessions.entry(side).or_insert(session);
    }
    assert_eq!(sessions.len(), 2);
    for (side, session) in &sessions {
        for (i, expected) in [(0, "miss"), (1, "semantic-hit; similarity=1.000")] {
            let r = e
                .send(
                    "sk-alice",
                    "chat/completions",
                    semantic_body(&format!("{side}-{i}")),
                    Some(("x-session-id", session)),
                )
                .await;
            assert_eq!(r.headers()["x-tokomotive-cache"], expected, "{side}");
            r.bytes().await.unwrap();
            e.st.flush().await;
        }
    }
    rules[0].id = "rule-two".into();
    sqlx::query("UPDATE routes SET rules=? WHERE name='cached'")
        .bind(serde_json::to_string(&rules).unwrap())
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e
        .send(
            "sk-alice",
            "chat/completions",
            semantic_body("other-rule"),
            Some(("x-session-id", sessions.values().next().unwrap())),
        )
        .await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    r.bytes().await.unwrap();
    e.st.flush().await;
}

#[tokio::test]
async fn exact_cache_isolates_route_rules_and_canary_sides_with_legacy_hits() {
    use tokomotive::cache::{self, Entry, Plan};
    let e = env("key", "openai").await;
    let table = e.st.router_table();
    // Freeze the pre-rule identity so a new no-match key cannot pass by storing
    // and reading only entries made with the new format.
    let legacy_identity = json!([
        null,
        "cached",
        "priority",
        {"enabled":true,"ttl_secs":60,"scope":"key"},
        [["p", "m", 1]],
        [],
        [["p", table.providers[0].base_url, "openai", "api_key", true]]
    ])
    .to_string();
    let legacy_body = json!({"model":"cached","messages":[{"role":"user","content":"legacy"}]});
    let key = tokomotive::db::lookup_key(&e.st.db, "sk-alice")
        .await
        .unwrap()
        .unwrap();
    let plan = Plan::new(
        &table.routes[0].cache,
        &legacy_identity,
        "/v1/chat/completions",
        &legacy_body,
        &axum::http::HeaderMap::new(),
        key.key_id,
        key.team_id,
    )
    .unwrap();
    let legacy_response = br#"{"id":"legacy","model":"m","choices":[{"message":{"role":"assistant","content":"old entry"},"finish_reason":"stop"}]}"#;
    let mut entry = Entry {
        body: String::new(),
        content_type: "application/json".into(),
        provider: "p".into(),
        upstream_model: Some("m".into()),
        model: Some("m".into()),
        usage: Default::default(),
        estimated: false,
    };
    entry.set_body(legacy_response);
    cache::store(
        &e.st.db,
        &e.st.secret_key,
        &plan,
        &e.st.settings().cache,
        &entry,
    )
    .await
    .unwrap();

    // Keep the Deployment set identical, forcing each side without random
    // cohort selection. Changing only the id must also create a separate entry.
    for (id, percent_bp, first) in [
        ("rule-a", 0, "miss"),
        ("rule-a", 10000, "miss"),
        ("rule-b", 10000, "miss"),
        ("rule-a", 0, "hit"),
        ("rule-a", 10000, "hit"),
    ] {
        let action = json!({"type":"deployments","deployments":[["p","m"]]});
        let rules = json!([{
            "id":id,"name":"Pilot","enabled":true,
            "conditions":{"header_name":"x-cohort","header_op":"equals","header_value":"pilot"},
            "action":action,
            "canary":{"percent_bp":percent_bp,"salt":"stable","action":action}
        }]);
        sqlx::query("UPDATE routes SET rules=? WHERE name='cached'")
            .bind(rules.to_string())
            .execute(&e.st.db)
            .await
            .unwrap();
        let legacy = e
            .send("sk-alice", "chat/completions", legacy_body.clone(), None)
            .await;
        assert_eq!(legacy.status(), 200);
        assert_eq!(legacy.headers()["x-tokomotive-cache"], "hit");
        assert_eq!(legacy.bytes().await.unwrap().as_ref(), legacy_response);
        e.st.flush().await;

        for expected in [first, "hit"] {
            let r = e
                .send(
                    "sk-alice",
                    "chat/completions",
                    body(),
                    Some(("x-cohort", "pilot")),
                )
                .await;
            assert_eq!(r.status(), 200);
            assert_eq!(
                r.headers()["x-tokomotive-cache"],
                expected,
                "{id}, canary percent {percent_bp}"
            );
            r.bytes().await.unwrap();
            e.st.flush().await;
        }
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn exact_hit_has_zero_cost_and_cached_ledger_usage() {
    use futures_util::StreamExt;
    let e = env("key", "openai").await;
    tokomotive::db::create_session(&e.st.db, "cache-live", 1)
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!("{}/admin/ledger/live", e.url))
        .header("cookie", "tokomotive_session=cache-live")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut live = response.bytes_stream();
    let first = e.send("sk-alice", "chat/completions", body(), None).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-tokomotive-cache"], "miss");
    let first = first.text().await.unwrap();
    e.st.flush().await;
    let first_event = tokio::time::timeout(std::time::Duration::from_secs(3), live.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&first_event)
            .unwrap()
            .contains("event: ledger")
    );
    let second = e.send("sk-alice", "chat/completions", body(), None).await;
    assert_eq!(second.headers()["x-tokomotive-cache"], "hit");
    assert_eq!(second.text().await.unwrap(), first);
    e.st.flush().await;
    let hit_event = tokio::time::timeout(std::time::Duration::from_secs(3), live.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&hit_event)
            .unwrap()
            .contains("cache hit")
    );
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    let row: (i64, i64, i64, i64) = sqlx::query_as("SELECT cache_hit,cost_micros,price_micros,input+output FROM ledger ORDER BY id DESC LIMIT 1").fetch_one(&e.st.db).await.unwrap();
    assert_eq!(row, (1, 0, 0, 5));
    assert!(
        tokomotive::db::recent_ledger(&e.st.db, Default::default(), 1)
            .await
            .unwrap()[0]
            .cache_hit
    );
    let metrics = reqwest::get(format!("{}/metrics", e.url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for metric in [
        "llm_cache_hits_total",
        "llm_cache_misses_total",
        "llm_cache_stored_bytes_total",
        "llm_cache_hit_bytes_total",
    ] {
        assert!(metrics.contains(metric), "{metric}");
    }
}

#[tokio::test]
async fn dashboard_has_cache_policy_and_limits() {
    let e = env("key", "openai").await;
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c
        .post(format!("{}/admin/login", e.url))
        .form(&[("password", "pw")])
        .send()
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|v| {
            v.to_str()
                .ok()
                .filter(|s| s.starts_with("tokomotive_session="))
        })
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for (path, field) in [
        ("/admin/routes", "name=\"cache_scope\""),
        ("/admin/routes", "name=\"semantic_embedding_model\""),
        ("/admin/routes", "name=\"semantic_threshold\""),
        ("/admin/routes", "name=\"semantic_scope\""),
        ("/admin/settings", "name=\"cache_max_entries\""),
        ("/admin", "Exact cache hit rate"),
        ("/admin", "Semantic cache hit rate"),
    ] {
        let r = c
            .get(format!("{}{path}", e.url))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains(field), "{path}: {field}");
    }
    let r = c
        .post(format!("{}/admin/routes/cached", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("name", "cached"),
            ("strategy", "priority"),
            ("deployment_provider", "p"),
            ("deployment_model", "m"),
            ("deployment_weight", "1"),
            ("cache_enabled", "on"),
            ("cache_ttl_secs", "42"),
            ("cache_scope", "global"),
            ("semantic_enabled", "on"),
            ("semantic_embedding_model", "p/m"),
            ("semantic_threshold", "0.97"),
            ("semantic_scope", "key"),
            ("semantic_ttl_secs", "90"),
            ("semantic_input", "conversation"),
            ("semantic_max_chars", "8000"),
            ("semantic_max_vectors", "40"),
            ("semantic_timeout_ms", "400"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, r#"{}"#, r.text().await.unwrap());
    assert_eq!(
        e.st.router_table().routes[0].cache.scope,
        tokomotive::cache::Scope::Global
    );
    assert_eq!(e.st.router_table().routes[0].cache.ttl_secs, 42);
    let policy = e.st.router_table().routes[0].cache.semantic.clone();
    assert!(policy.enabled);
    assert_eq!(policy.embedding_model, "p/m");
    assert_eq!(policy.threshold, 0.97);
    assert_eq!(policy.input.as_str(), "conversation");
    assert_eq!(policy.max_chars, 8000);
    assert_eq!(policy.max_vectors, 40);
    assert_eq!(policy.timeout_ms, 400);
    assert_eq!(policy.ttl_secs, 90);
    let r = c
        .post(format!("{}/admin/settings", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("upstream_timeout_secs", "30"),
            ("session_days", "7"),
            ("failure_threshold", "3"),
            ("cooldown_secs", "30"),
            ("max_attempts", "4"),
            ("cache_max_entries", "20"),
            ("cache_max_bytes", "20000"),
            ("cache_max_entry_bytes", "2000"),
            ("cache_hit_price_micros", "7"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, r#"{}"#, r.text().await.unwrap());
    assert_eq!(e.st.settings().cache.max_entries, 20);
    assert_eq!(e.st.settings().cache.hit_price_micros, 7);
}

#[tokio::test]
async fn translated_truncated_stream_does_not_populate_cache() {
    let e = env("key", "openai").await;
    let body = json!({"model":"cached","max_tokens":20,"stream":true,"messages":[{"role":"user","content":"partial"}]});
    for _ in 0..2 {
        let r = e.send("sk-alice", "messages", body.clone(), None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.text().await.unwrap();
        e.st.flush().await;
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bypass_controls_and_key_isolation() {
    let e = env("key", "openai").await;
    let count = || e.calls.load(Ordering::SeqCst);
    let send = |key, body, control| e.send(key, "chat/completions", body, control);
    let r = send("sk-alice", body(), Some(("Cache-Control", "no-store"))).await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "bypass");
    r.text().await.unwrap();
    assert_eq!(
        send("sk-alice", body(), None).await.headers()["x-tokomotive-cache"],
        "miss"
    );
    assert_eq!(
        send("sk-bob", body(), None).await.headers()["x-tokomotive-cache"],
        "miss"
    );
    assert_eq!(
        send("sk-alice", body(), None).await.headers()["x-tokomotive-cache"],
        "hit"
    );
    assert_eq!(
        send(
            "sk-alice",
            body(),
            Some(("Cache-Control", "max-age=0, NO-CACHE"))
        )
        .await
        .headers()["x-tokomotive-cache"],
        "bypass"
    );
    assert_eq!(
        send("sk-alice", body(), Some(("x-tokomotive-cache", "bypass")))
            .await
            .headers()["x-tokomotive-cache"],
        "bypass"
    );
    let mut same = body();
    same["user"] = json!("another attribution");
    same["metadata"] = json!({"tag":"v"});
    assert_eq!(
        send("sk-alice", same, None).await.headers()["x-tokomotive-cache"],
        "hit"
    );
    let mut n = body();
    n["n"] = json!(2);
    assert_eq!(
        send("sk-alice", n, None).await.headers()["x-tokomotive-cache"],
        "bypass"
    );
    assert_eq!(count(), 6);
    let alice = tokomotive::db::lookup_key(&e.st.db, "sk-alice")
        .await
        .unwrap()
        .unwrap();
    let (_, other) = tokomotive::db::create_key(&e.st.db, alice.user_id, "second", None)
        .await
        .unwrap();
    assert_eq!(
        send(&other, body(), None).await.headers()["x-tokomotive-cache"],
        "miss"
    );
    e.st.flush().await;
}

#[tokio::test]
async fn team_scope_shares_within_team_only_and_missing_team_bypasses() {
    let e = env("team", "openai").await;
    assert_eq!(
        e.send("sk-alice", "chat/completions", body(), None)
            .await
            .headers()["x-tokomotive-cache"],
        "bypass"
    );
    sqlx::query("INSERT INTO teams(id,name,created_at) VALUES(1,'a',0),(2,'b',0)")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET team_id=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        e.send("sk-alice", "chat/completions", body(), None)
            .await
            .headers()["x-tokomotive-cache"],
        "miss"
    );
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .headers()["x-tokomotive-cache"],
        "hit"
    );
    sqlx::query("UPDATE users SET team_id=2 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .headers()["x-tokomotive-cache"],
        "miss"
    );
    assert_eq!(e.calls.load(Ordering::SeqCst), 3);
    e.st.flush().await;
}

#[tokio::test]
async fn global_hit_obeys_allowlists_budgets_rpm_and_configured_price() {
    let e = env("global", "openai").await;
    e.send("sk-alice", "chat/completions", body(), None)
        .await
        .text()
        .await
        .unwrap();
    e.st.flush().await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.cache.hit_price_micros',7) WHERE key='runtime_settings'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"other\"]' WHERE user_id=(SELECT id FROM users WHERE name='bob')").execute(&e.st.db).await.unwrap();
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .status(),
        403
    );
    sqlx::query("UPDATE virtual_keys SET allowed_models=NULL")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET allowed_providers='[\"other\"]' WHERE user_id=(SELECT id FROM users WHERE name='bob')").execute(&e.st.db).await.unwrap();
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .status(),
        403
    );
    sqlx::query("UPDATE virtual_keys SET allowed_providers=NULL")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET budget_micros=6 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .status(),
        402
    );
    sqlx::query("UPDATE users SET budget_micros=100,rpm_limit=1 WHERE name='bob'")
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e.send("sk-bob", "chat/completions", body(), None).await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "hit");
    r.text().await.unwrap();
    e.st.flush().await;
    let amounts: (i64, i64, i64) = sqlx::query_as(
        "SELECT cost_micros,price_micros,cache_hit FROM ledger ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&e.st.db)
    .await
    .unwrap();
    assert_eq!(amounts, (0, 7, 1));
    assert_eq!(
        e.send("sk-bob", "chat/completions", body(), None)
            .await
            .status(),
        429
    );
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE users SET rpm_limit=NULL; ")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE virtual_keys SET tpm_limit=1 WHERE user_id=(SELECT id FROM users WHERE name='bob')",
    )
    .execute(&e.st.db)
    .await
    .unwrap();
    for _ in 0..2 {
        let r = e.send("sk-bob", "chat/completions", body(), None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "hit");
        r.text().await.unwrap();
        e.st.flush().await;
    }
}

#[tokio::test]
async fn streaming_replay_keeps_client_format_and_usage_variant() {
    for (kind, endpoint) in [
        ("openai", "chat/completions"),
        ("anthropic", "messages"),
        ("openai", "messages"),
        ("anthropic", "chat/completions"),
    ] {
        let e = env("key", kind).await;
        let mut b = body();
        b["stream"] = json!(true);
        if endpoint == "messages" {
            b["max_tokens"] = json!(20);
        } else {
            b["stream_options"] = json!({"include_usage":true});
        }
        let r = e.send("sk-alice", endpoint, b.clone(), None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        let original = r.text().await.unwrap();
        e.st.flush().await;
        let r = e.send("sk-alice", endpoint, b.clone(), None).await;
        assert_eq!(
            r.headers()["x-tokomotive-cache"],
            "hit",
            "{kind} {endpoint}: {original}"
        );
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let replay = r.text().await.unwrap();
        assert_eq!(original, replay);
        assert!(replay.contains("usage"));
        assert!(replay.contains(if endpoint == "messages" {
            "message_stop"
        } else {
            "[DONE]"
        }));
        if endpoint == "chat/completions" {
            b["stream_options"]["include_usage"] = json!(false);
            let r = e.send("sk-alice", endpoint, b.clone(), None).await;
            assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
            r.text().await.unwrap();
        }
        b["stream"] = json!(false);
        let r = e.send("sk-alice", endpoint, b, None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.text().await.unwrap();
        e.st.flush().await;
    }
}

#[tokio::test]
async fn storage_expiry_encryption_caps_and_oldest_eviction() {
    use tokomotive::cache::{self, Entry, Limits, Plan};
    let e = env("key", "openai").await;
    let limits = Limits {
        max_entries: 2,
        ..Default::default()
    };
    let plan = |key: &str| Plan {
        key: key.into(),
        ttl_secs: 60,
        lookup: true,
    };
    let mut entry = Entry {
        body: String::new(),
        content_type: "application/json".into(),
        provider: "p".into(),
        upstream_model: Some("m".into()),
        model: Some("m".into()),
        usage: Default::default(),
        estimated: false,
    };
    entry.set_body(b"sensitive secret response");
    cache::store(&e.st.db, &e.st.secret_key, &plan("a"), &limits, &entry)
        .await
        .unwrap();
    let encrypted: String = sqlx::query_scalar("SELECT body FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert!(!encrypted.contains("sensitive"));
    assert!(!encrypted.contains("provider"));
    assert_eq!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("a"), &limits)
            .await
            .unwrap()
            .unwrap()
            .bytes()
            .unwrap(),
        b"sensitive secret response"
    );
    for key in ["b", "c"] {
        cache::store(&e.st.db, &e.st.secret_key, &plan(key), &limits, &entry)
            .await
            .unwrap();
    }
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("a"), &limits)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("b"), &limits)
            .await
            .unwrap()
            .is_some()
    );
    cache::maintain(
        &e.st.db,
        &Limits {
            max_entries: 1,
            ..limits.clone()
        },
    )
    .await
    .unwrap();
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("b"), &limits)
            .await
            .unwrap()
            .is_none()
    );
    let tiny = Limits {
        max_entry_bytes: 1,
        ..limits.clone()
    };
    cache::store(&e.st.db, &e.st.secret_key, &plan("large"), &tiny, &entry)
        .await
        .unwrap();
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("large"), &limits)
            .await
            .unwrap()
            .is_none()
    );
    let bytes = Limits {
        max_bytes: 600,
        ..limits.clone()
    };
    cache::store(&e.st.db, &e.st.secret_key, &plan("d"), &bytes, &entry)
        .await
        .unwrap();
    let total: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(body_bytes),0) FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert!(total <= 600);
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("d"), &limits)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("UPDATE response_cache SET expires_ms=0")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan("d"), &limits)
            .await
            .unwrap()
            .is_none()
    );
    cache::purge(&e.st.db).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_cache")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn store_enforces_byte_cap_when_more_than_500_evictions_are_needed() {
    use tokomotive::cache::{self, Entry, Limits, Plan};
    let e = env("key", "openai").await;
    let limits = Limits {
        max_entries: 2000,
        max_bytes: 1024 * 1024,
        max_entry_bytes: 1024 * 1024,
        ..Default::default()
    };
    sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO response_cache(key,body,body_bytes,created_ms,expires_ms) SELECT 'old-'||x,zeroblob(1024),1024,0,9223372036854775807 FROM n")
        .execute(&e.st.db).await.unwrap();
    let plan = Plan {
        key: "large-new".into(),
        ttl_secs: 60,
        lookup: true,
    };
    let mut entry = Entry {
        body: String::new(),
        content_type: "application/json".into(),
        provider: "p".into(),
        upstream_model: Some("m".into()),
        model: Some("m".into()),
        usage: Default::default(),
        estimated: false,
    };
    // The encrypted envelope includes two base64 layers: about 900 KiB.
    let bytes = vec![b'x'; 506 * 1024];
    entry.set_body(&bytes);
    cache::store(&e.st.db, &e.st.secret_key, &plan, &limits, &entry)
        .await
        .unwrap();
    let size: i64 =
        sqlx::query_scalar("SELECT body_bytes FROM response_cache WHERE key='large-new'")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert!((890 * 1024..910 * 1024).contains(&size), "{size}");
    let total: i64 = sqlx::query_scalar("SELECT SUM(body_bytes) FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert!(
        total <= limits.max_bytes as i64,
        "stored {total} bytes, cap {}",
        limits.max_bytes
    );
    assert_eq!(
        cache::lookup(&e.st.db, &e.st.secret_key, &plan, &limits)
            .await
            .unwrap()
            .unwrap()
            .bytes()
            .unwrap(),
        bytes
    );
}

#[tokio::test]
async fn responses_embeddings_tools_errors_and_disabled_route() {
    let e = env("key", "openai").await;
    for (endpoint, b) in [
        ("responses", json!({"model":"cached","input":"hi"})),
        (
            "responses",
            json!({"model":"cached","input":"hi","stream":true}),
        ),
        ("embeddings", json!({"model":"cached","input":["hello"]})),
        (
            "chat/completions",
            json!({"model":"cached","messages":[{"role":"tool","tool_call_id":"id","content":"result"}],"tools":[{"type":"function","function":{"name":"test","parameters":{"type":"object"}}}]}),
        ),
    ] {
        let r = e.send("sk-alice", endpoint, b.clone(), None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        let bytes = r.bytes().await.unwrap();
        e.st.flush().await;
        let r = e.send("sk-alice", endpoint, b, None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "hit");
        assert_eq!(r.bytes().await.unwrap(), bytes);
        e.st.flush().await;
    }
    let mut b = body();
    b["messages"][0]["content"] = json!("error");
    for _ in 0..2 {
        assert_eq!(
            e.send("sk-alice", "chat/completions", b.clone(), None)
                .await
                .status(),
            400
        );
    }
    sqlx::query(
        "UPDATE routes SET config_json=json_set(config_json,'$.cache.enabled',json('false'))",
    )
    .execute(&e.st.db)
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    assert_eq!(
        e.send(
            "sk-alice",
            "responses",
            json!({"model":"cached","input":"hi"}),
            None
        )
        .await
        .headers()["x-tokomotive-cache"],
        "bypass"
    );
    assert_eq!(e.calls.load(Ordering::SeqCst), 7);
    e.st.flush().await;
}

#[tokio::test]
async fn incomplete_json_response_is_not_cached() {
    let e = env("key", "openai").await;
    for _ in 0..2 {
        let r = e
            .send(
                "sk-alice",
                "responses",
                json!({"model":"cached","input":"incomplete"}),
                None,
            )
            .await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.text().await.unwrap();
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 2);
    e.st.flush().await;
}

#[tokio::test]
async fn aborted_stream_and_http_size_cap_do_not_populate_cache() {
    let e = env("key", "openai").await;
    let mut b = body();
    b["stream"] = json!(true);
    b["messages"][0]["content"] = json!("abort");
    let r = e
        .send("sk-alice", "chat/completions", b.clone(), None)
        .await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    drop(r);
    // Hold the terminal event until after the client has closed its socket.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    e.stream_gate.notify_one();
    e.st.flush().await;
    let r = e
        .send("sk-alice", "chat/completions", b.clone(), None)
        .await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
    e.stream_gate.notify_one();
    r.text().await.unwrap();
    e.st.flush().await;
    assert_eq!(
        e.send("sk-alice", "chat/completions", b, None)
            .await
            .headers()["x-tokomotive-cache"],
        "hit"
    );
    sqlx::query("UPDATE settings SET value=json_set(value,'$.cache.max_entry_bytes',1) WHERE key='runtime_settings'").execute(&e.st.db).await.unwrap();
    e.st.reload_settings().await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            e.send("sk-alice", "chat/completions", body(), None)
                .await
                .headers()["x-tokomotive-cache"],
            "miss"
        );
    }
    assert_eq!(e.calls.load(Ordering::SeqCst), 4);
    e.st.flush().await;
}

#[test]
fn canonical_request_identity_preserves_output_fields_and_scope() {
    use tokomotive::cache::{Plan, Policy, Scope};
    let p = Policy {
        semantic: Default::default(),
        enabled: true,
        ..Default::default()
    };
    let headers = axum::http::HeaderMap::new();
    assert!(
        Plan::new(
            &p,
            "route",
            "chat_completions",
            &json!({"model":"m","n":2.0}),
            &headers,
            1,
            None
        )
        .is_none()
    );
    let key = |p: &Policy, endpoint: &str, b: Value, k, t| {
        Plan::new(p, "resolved route", endpoint, &b, &headers, k, t).map(|p| p.key)
    };
    let a: Value = serde_json::from_str(
        r#"{"model":"m","messages":[{"content":"hi","role":"user"}],"user":"one"}"#,
    )
    .unwrap();
    let b: Value=serde_json::from_str(r#"{"user":"two","messages":[{"role":"user","content":"hi"}],"model":"m","stream":false,"metadata":{"tag":1}}"#).unwrap();
    assert_eq!(
        key(&p, "messages", a.clone(), 1, None),
        key(&p, "messages", b.clone(), 1, None)
    );
    let mut changed = b.clone();
    changed["temperature"] = json!(0.1);
    assert_ne!(
        key(&p, "messages", a.clone(), 1, None),
        key(&p, "messages", changed, 1, None)
    );
    let mut nested = b.clone();
    nested["messages"][0]["metadata"] = json!("tool content");
    assert_ne!(
        key(&p, "messages", a.clone(), 1, None),
        key(&p, "messages", nested, 1, None)
    );
    assert_ne!(
        key(&p, "messages", a.clone(), 1, None),
        key(&p, "responses", a.clone(), 1, None)
    );
    assert_ne!(
        key(&p, "messages", a.clone(), 1, None),
        key(&p, "messages", a.clone(), 2, None)
    );
    let team = Policy {
        scope: Scope::Team,
        ..p.clone()
    };
    assert_ne!(
        key(&team, "messages", a.clone(), 1, Some(1)),
        key(&team, "messages", a.clone(), 1, Some(2))
    );
    assert_ne!(
        key(&p, "messages", a.clone(), 1, None),
        key(&team, "messages", a, 1, Some(1))
    );
}

#[test]
fn incomplete_success_envelopes_and_empty_terminal_streams_are_rejected() {
    use tokomotive::cache::{self, StreamBody};
    for (endpoint, body) in [
        ("chat_completions", r#"{"choices":[]}"#),
        (
            "messages",
            r#"{"type":"message","content":[],"stop_reason":null}"#,
        ),
        ("embeddings", r#"{"object":"list"}"#),
    ] {
        assert!(
            !cache::complete_json(endpoint, body.as_bytes()),
            "{endpoint}"
        );
    }
    for (endpoint, body) in [
        ("chat_completions", "data: [DONE]\n\n"),
        (
            "messages",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    ] {
        let mut stream = StreamBody::new(1000);
        stream.feed(body.as_bytes());
        assert!(stream.complete(endpoint).is_none(), "{endpoint}");
    }
}

#[tokio::test]
async fn endpoint_query_and_encrypted_identity_are_isolated() {
    use tokomotive::cache::{self, Entry, Limits, Plan};
    let e = env("key", "openai").await;
    for endpoint in [
        "chat/completions?version=one",
        "chat/completions?version=two",
    ] {
        let r = e.send("sk-alice", endpoint, body(), None).await;
        assert_eq!(r.headers()["x-tokomotive-cache"], "miss");
        r.text().await.unwrap();
    }
    let plan = |key: &str| Plan {
        key: key.into(),
        ttl_secs: 60,
        lookup: true,
    };
    let mut entry = Entry {
        body: String::new(),
        content_type: "application/json".into(),
        provider: "p".into(),
        upstream_model: Some("m".into()),
        model: Some("m".into()),
        usage: Default::default(),
        estimated: false,
    };
    entry.set_body(b"private scoped response");
    cache::store(
        &e.st.db,
        &e.st.secret_key,
        &plan("scope-one"),
        &Limits::default(),
        &entry,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO response_cache(key,body,body_bytes,created_ms,expires_ms) SELECT 'scope-two',body,body_bytes,created_ms,expires_ms FROM response_cache WHERE key='scope-one'").execute(&e.st.db).await.unwrap();
    assert!(
        cache::lookup(
            &e.st.db,
            &e.st.secret_key,
            &plan("scope-two"),
            &Limits::default()
        )
        .await
        .is_err()
    );
    e.st.flush().await;
}

/// Last-user mode embeds only the last user message, but the earlier
/// conversation must still separate entries: in a shared scope, a different
/// history with the same last question must not reuse another caller's answer.
#[tokio::test]
async fn semantic_last_user_mode_keeps_earlier_context_in_the_bucket() {
    let e = semantic_env("global").await;
    let with_history = |history: &str| {
        json!({"model":"cached","messages":[
            {"role":"user","content":history},
            {"role":"assistant","content":"noted"},
            {"role":"user","content":"hello"}
        ]})
    };
    e.send(
        "sk-alice",
        "chat/completions",
        with_history("context one"),
        None,
    )
    .await
    .bytes()
    .await
    .unwrap();
    e.st.flush().await;
    let r = e
        .send(
            "sk-bob",
            "chat/completions",
            with_history("context two"),
            None,
        )
        .await;
    let tag = r.headers()["x-tokomotive-cache"]
        .to_str()
        .unwrap()
        .to_owned();
    r.bytes().await.unwrap();
    assert_eq!(tag, "miss", "a different history reused an answer");
    e.st.flush().await;
    let r = e
        .send(
            "sk-bob",
            "chat/completions",
            with_history("context one"),
            None,
        )
        .await;
    assert!(
        r.headers()["x-tokomotive-cache"]
            .to_str()
            .unwrap()
            .contains("hit"),
        "the same history must still hit"
    );
}
