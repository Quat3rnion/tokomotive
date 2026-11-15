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
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    stream_gate: Arc<tokio::sync::Notify>,
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
        return Json(json!({"object":"list","model":"m","data":[{"object":"embedding","index":0,"embedding":[0.25,0.5]}],"usage":{"prompt_tokens":2,"total_tokens":2}})).into_response();
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
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(answer))
        .route("/v1/messages", post(answer))
        .route("/v1/responses", post(answer))
        .route("/v1/embeddings", post(answer))
        .with_state(Mock {
            calls: calls.clone(),
            stream_gate: stream_gate.clone(),
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

#[tokio::test]
async fn exact_hit_has_zero_cost_and_cached_ledger_usage() {
    let e = env("key", "openai").await;
    let first = e.send("sk-alice", "chat/completions", body(), None).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-tokomotive-cache"], "miss");
    let first = first.text().await.unwrap();
    e.st.flush().await;
    let second = e.send("sk-alice", "chat/completions", body(), None).await;
    assert_eq!(second.headers()["x-tokomotive-cache"], "hit");
    assert_eq!(second.text().await.unwrap(), first);
    e.st.flush().await;
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
        ("/admin/settings", "name=\"cache_max_entries\""),
        ("/admin", "Exact cache hit rate"),
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
