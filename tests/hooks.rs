use axum::{Json, extract::State, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokomotive::{config::Config, state::AppState};

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(HeaderMap, Value, Vec<u8>)>>,
    upstream: Mutex<Vec<Value>>,
    reply: Mutex<Value>,
    block_events: AtomicBool,
    event_gate: tokio::sync::Notify,
}
struct Env {
    dir: tempfile::TempDir,
    st: AppState,
    url: String,
    hook: String,
    mock: Arc<Mock>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn webhook(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    bytes: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    m.calls
        .lock()
        .unwrap()
        .push((headers, value.clone(), bytes.to_vec()));
    let notified = m.event_gate.notified();
    if value["stage"] == "event" && m.block_events.load(Ordering::SeqCst) {
        notified.await;
    }
    let reply = m.reply.lock().unwrap().clone();
    if let Some(delay) = reply["delay_ms"].as_u64() {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    match reply["mock"].as_str() {
        Some("redirect") => {
            (axum::http::StatusCode::FOUND, [("location", "/hook")]).into_response()
        }
        Some("oversized") => (
            [("content-type", "application/json")],
            "x".repeat(1024 * 1024 + 1),
        )
            .into_response(),
        _ => Json(reply).into_response(),
    }
}
async fn upstream(State(m): State<Arc<Mock>>, Json(v): Json<Value>) -> axum::response::Response {
    use axum::response::IntoResponse;
    m.upstream.lock().unwrap().push(v.clone());
    if v["stream"] == true {
        return (
            [("content-type", "text/event-stream")],
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
        ).into_response();
    }
    Json(json!({"id":"c","model":"m","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}})).into_response()
}
async fn embedding(Json(v): Json<Value>) -> Json<Value> {
    Json(
        json!({"model":v["model"],"data":[{"index":0,"embedding":[1.0,0.0]}],"usage":{"prompt_tokens":2,"total_tokens":2}}),
    )
}
async fn env() -> Env {
    let dir = tempfile::tempdir_in("/arboretum/toko-hooks-tmp").unwrap();
    let mock = Arc::new(Mock::default());
    *mock.reply.lock().unwrap() = json!({"decision":"continue"});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/hook", post(webhook))
        .route("/v1/chat/completions", post(upstream))
        .route("/v1/embeddings", post(embedding))
        .with_state(mock.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: Config = toml::from_str(&format!(
        "[server]\ndatabase='sqlite::memory:'\nmode='simple'\nadmin_password='password123'\n[[providers]]\nname='p'\nkind='openai'\nbase_url='http://{addr}'\napi_key='test'\n[[models]]\nname='m'\nprovider='p'\ncost={{input=1,output=2}}\n[[routes]]\nname='chat'\ndeployments=[{{provider='p',model='m'}}]\ncache={{enabled=true,ttl_secs=60}}\n[[users]]\nname='alice'\nkeys=['sk-alice']"
    )).unwrap();
    let mut cfg = cfg;
    cfg.server.database = format!("sqlite://{}", dir.path().join("hooks.sqlite").display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::db::create_session(&st.db, "hooks-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        dir,
        st,
        url,
        hook: format!("http://{addr}/hook"),
        mock,
        tasks: vec![task, proxy],
    }
}
impl Env {
    async fn guardrail(&self, action: &str, direction: &str) {
        let rules = json!([{"name":"email","kind":{"type":"detector","detector":"email"},"action":action,"direction":direction}]);
        let id = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('email-policy',?)")
            .bind(rules.to_string())
            .execute(&self.st.db)
            .await
            .unwrap()
            .last_insert_rowid();
        sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
            .bind(id)
            .execute(&self.st.db)
            .await
            .unwrap();
    }
    async fn rules(&self, rules: Value) {
        sqlx::query("UPDATE routes SET rules=? WHERE name='chat'")
            .bind(rules.to_string())
            .execute(&self.st.db)
            .await
            .unwrap();
    }
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }
    async fn admin(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.client()
            .post(format!("{}{path}", self.url))
            .header("cookie", "tokomotive_session=hooks-session")
            .header("origin", &self.url)
            .form(form)
            .send()
            .await
            .unwrap()
    }
    async fn create(&self, name: &str, stages: &[&str]) -> (i64, String) {
        let mut form = vec![
            ("name", name),
            ("url", &self.hook),
            ("allow_private", "on"),
            ("enabled", "on"),
        ];
        for stage in stages {
            form.push((stage, "on"));
        }
        let r = self.admin("/admin/hooks", &form).await;
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        (
            v["id"].as_i64().unwrap(),
            v["secret"].as_str().unwrap().into(),
        )
    }
    async fn attach(&self, id: i64, form: &[(&str, &str)]) {
        let r = self.admin(&format!("/admin/hooks/{id}/attach"), form).await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    }
    async fn send(&self, text: &str, stream: bool) -> reqwest::Response {
        self.client().post(format!("{}/v1/chat/completions", self.url)).bearer_auth("sk-alice")
            .json(&json!({"model":"chat","messages":[{"role":"user","content":text}],"max_tokens":100,"stream":stream})).send().await.unwrap()
    }
}

#[tokio::test]
async fn final_hook_body_is_blocked_or_redacted_by_request_guardrails() {
    for action in ["block", "redact"] {
        let e = env().await;
        e.guardrail(action, "request").await;
        let (id, _) = e.create("insert-email", &["pre_request"]).await;
        e.attach(id, &[("route_name", "chat")]).await;
        *e.mock.reply.lock().unwrap() = json!({"decision":"modify","patch":{"messages":[{"role":"user","content":"alice@example.com"}]}});
        let r = e.send("safe", false).await;
        assert_eq!(r.status(), if action == "block" { 400 } else { 200 });
        r.bytes().await.unwrap();
        e.st.flush().await;
        let upstream = e.mock.upstream.lock().unwrap();
        if action == "block" {
            assert!(upstream.is_empty());
        } else {
            assert_eq!(upstream[0]["messages"][0]["content"], "[REDACTED:email]");
        }
        assert_eq!(e.mock.calls.lock().unwrap().len(), 1, "never replay hooks");
    }
}

#[tokio::test]
async fn final_hook_tools_and_stream_changes_cannot_bypass_route_rules() {
    for (condition, patch) in [
        (
            json!({"has_tools":true}),
            json!({"tools":[{"type":"function","function":{"name":"run"}}]}),
        ),
        (json!({"stream":true}), json!({"stream":true})),
    ] {
        let e = env().await;
        e.rules(json!([{"id":"restricted","name":"restricted","enabled":true,"conditions":condition,"action":{"type":"reject","message":"Not allowed by Route policy."}}])).await;
        let (id, _) = e.create("insert-restricted", &["pre_request"]).await;
        e.attach(id, &[("route_name", "chat")]).await;
        *e.mock.reply.lock().unwrap() = json!({"decision":"modify","patch":patch});
        let r = e.send("safe", false).await;
        assert_eq!(r.status(), 403);
        assert!(
            r.text()
                .await
                .unwrap()
                .contains("Not allowed by Route policy.")
        );
        e.st.flush().await;
        assert!(e.mock.upstream.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn final_hook_body_cannot_silently_change_the_chosen_deployments() {
    let e = env().await;
    e.rules(json!([{"id":"tools","name":"tools","enabled":true,"conditions":{"has_tools":true},"action":{"type":"deployments","deployments":[["p","m"]]}}])).await;
    let (id, _) = e.create("insert-tools", &["pre_request"]).await;
    e.attach(id, &[("route_name", "chat")]).await;
    *e.mock.reply.lock().unwrap() = json!({"decision":"modify","patch":{"tools":[{"type":"function","function":{"name":"run"}}]}});
    let r = e.send("safe", false).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("Route rule"));
    e.st.flush().await;
    assert!(e.mock.upstream.lock().unwrap().is_empty());
}

#[tokio::test]
async fn pre_request_continue_is_signed_and_keeps_identity() {
    use hmac::{Hmac, Mac};
    let e = env().await;
    let (id, secret) = e.create("verify", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    let r = e.send("hello", false).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (headers, v, body) = &calls[0];
    assert_eq!(v["version"], 1);
    assert_eq!(v["stage"], "pre_request");
    assert_eq!(v["data"]["body"]["model"], "chat");
    assert_eq!(v["data"]["user"], "alice");
    assert_eq!(headers["x-tokomotive-hook"], "verify");
    let signature = headers["x-tokomotive-signature"].to_str().unwrap();
    let (timestamp, signature) = signature
        .strip_prefix("t=")
        .unwrap()
        .split_once(",v1=")
        .unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&hex::decode(signature).unwrap()).unwrap();
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pre_request_modify_revalidates_model_permissions_and_limits() {
    let e = env().await;
    let (id, _) = e.create("modify", &["pre_request"]).await;
    e.attach(id, &[("route_name", "chat")]).await;
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"modify","patch":{"messages":[{"role":"user","content":"changed"}]}});
    assert_eq!(e.send("before", false).await.status(), 200);
    assert_eq!(
        e.mock.upstream.lock().unwrap()[0]["messages"][0]["content"],
        "changed"
    );
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"chat\"]'")
        .execute(&e.st.db)
        .await
        .unwrap();
    *e.mock.reply.lock().unwrap() = json!({"decision":"modify","patch":{"model":"p/not-allowed"}});
    let r = e.send("model", false).await;
    assert!(r.status().is_client_error());
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 1);
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"modify","patch":{"max_completion_tokens":99}});
    assert_eq!(e.send("limits", false).await.status(), 400);
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pre_request_reject_has_ledger_and_no_upstream_cost() {
    let e = env().await;
    let (id, _) = e.create("deny", &["pre_request", "event"]).await;
    e.attach(id, &[("global", "on")]).await;
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"reject","status":422,"message":"Denied by policy."});
    let r = e.send("secret prompt", false).await;
    assert_eq!(r.status(), 422);
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["hook"], "deny");
    e.st.flush().await;
    assert!(e.mock.upstream.lock().unwrap().is_empty());
    let rows = tokomotive::db::recent_ledger(&e.st.db, Default::default(), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 422);
    assert_eq!(rows[0].cost_micros, 0);
    assert!(rows[0].hook_summary.contains("rejected by hook deny"));
    let calls = e.mock.calls.lock().unwrap();
    let event = &calls
        .iter()
        .find(|(_, v, _)| v["stage"] == "event")
        .unwrap()
        .1;
    assert_eq!(event["data"]["ledger"]["status"], 422);
    assert!(!event.to_string().contains("secret prompt"));
}

#[tokio::test]
async fn post_response_modify_and_reject_keep_upstream_cost_and_disable_cache_store() {
    let e = env().await;
    let (id, _) = e.create("answer-policy", &["post_response"]).await;
    e.attach(id, &[("route_name", "chat")]).await;
    *e.mock.reply.lock().unwrap() = json!({"decision":"modify","patch":{"choices":[{"message":{"role":"assistant","content":"changed"},"finish_reason":"stop"}]}});
    let r = e.send("one", false).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "changed"
    );
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"reject","status":451,"message":"Response denied."});
    let r = e.send("one", false).await;
    assert_eq!(r.status(), 451);
    r.bytes().await.unwrap();
    e.st.flush().await;
    let rows = tokomotive::db::recent_ledger(&e.st.db, Default::default(), 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.cost_micros == 8));
    assert_eq!(rows[0].status, 451);
    assert!(
        rows[0]
            .hook_summary
            .contains("rejected by hook answer-policy")
    );
    assert!(
        rows[1]
            .hook_summary
            .contains("modified by hook answer-policy")
    );
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 2);
    let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(cached, 0);
}

#[tokio::test]
async fn streaming_skips_post_response_but_delivers_final_event() {
    let e = env().await;
    let (id, _) = e.create("stream-policy", &["post_response", "event"]).await;
    e.attach(id, &[("global", "on")]).await;
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"reject","status":403,"message":"Must not run for streams"});
    let r = e.send("stream prompt", true).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("[DONE]"));
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["stage"], "event");
    assert_eq!(calls[0].1["data"]["ledger"]["streamed"], true);
    assert_eq!(calls[0].1["data"]["ledger"]["quote"]["cost_micros"], 8);
    assert!(!calls[0].1.to_string().contains("stream prompt"));
}

#[tokio::test]
async fn timeout_fail_open_and_fail_closed_and_bounded_responses() {
    let e = env().await;
    let (id, _) = e.create("timeouts", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    sqlx::query("UPDATE hooks SET timeout_ms=40 WHERE id=?")
        .bind(id)
        .execute(&e.st.db)
        .await
        .unwrap();
    *e.mock.reply.lock().unwrap() = json!({"decision":"continue","delay_ms":150});
    let r = e.send("open", false).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    sqlx::query("UPDATE hooks SET fail_closed=1 WHERE id=?")
        .bind(id)
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(e.send("closed", false).await.status(), 502);
    for mock in ["redirect", "oversized"] {
        *e.mock.reply.lock().unwrap() = json!({"mock":mock});
        assert_eq!(e.send(mock, false).await.status(), 502);
    }
    e.st.flush().await;
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 1);
    let outcomes: Vec<String> = sqlx::query_scalar("SELECT outcome FROM hook_calls ORDER BY id")
        .fetch_all(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        outcomes,
        [
            "timeout",
            "rejected_timeout",
            "rejected_error",
            "rejected_error"
        ]
    );
    assert_eq!(
        e.mock.calls.lock().unwrap().len(),
        4,
        "redirect was followed"
    );
}

#[tokio::test]
async fn public_http_and_private_without_opt_in_are_refused() {
    let e = env().await;
    for (url, private) in [("http://1.1.1.1/hook", "on"), (e.hook.as_str(), "")] {
        let r = e
            .admin(
                "/admin/hooks",
                &[
                    ("name", "unsafe"),
                    ("url", url),
                    ("pre_request", "on"),
                    ("allow_private", private),
                ],
            )
            .await;
        assert!(r.status().is_client_error());
    }
    assert!(e.mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn egress_proxy_does_not_bypass_private_ip_or_local_dns_checks() {
    let e = env().await;
    let (id, _) = e.create("proxy-network-policy", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    let proxy = e.hook.strip_suffix("/hook").unwrap();
    tokomotive::settings::put(
        &e.st.db,
        "egress_proxy",
        &e.st.secret_key.encrypt(proxy).unwrap(),
    )
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    assert!(e.st.egress.label(None).is_some());
    for url in [e.hook.clone(), e.hook.replace("127.0.0.1", "localhost")] {
        let r = e
            .admin(
                "/admin/hooks",
                &[
                    ("name", "denied-private"),
                    ("url", &url),
                    ("pre_request", "on"),
                ],
            )
            .await;
        assert!(r.status().is_client_error());
        // Also check the live send path, not only dashboard validation.
        sqlx::query("UPDATE hooks SET url=?,allow_private=0,fail_closed=1 WHERE id=?")
            .bind(&url)
            .bind(id)
            .execute(&e.st.db)
            .await
            .unwrap();
        let r = e.send("private target", false).await;
        assert_eq!(r.status(), 502);
        r.bytes().await.unwrap();
        e.st.flush().await;
    }
    assert!(e.mock.calls.lock().unwrap().is_empty());
    assert!(e.mock.upstream.lock().unwrap().is_empty());
}

#[tokio::test]
async fn modified_request_changes_exact_cache_key() {
    let e = env().await;
    let (id, _) = e.create("cache-patch", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"modify","patch":{"messages":[{"role":"user","content":"B"}]}});
    e.send("A", false).await.bytes().await.unwrap();
    e.st.flush().await;
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"modify","patch":{"messages":[{"role":"user","content":"C"}]}});
    e.send("A", false).await.bytes().await.unwrap();
    e.st.flush().await;
    *e.mock.reply.lock().unwrap() = json!({"decision":"continue"});
    let r = e.send("B", false).await;
    assert_eq!(r.headers()["x-tokomotive-cache"], "hit");
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert_eq!(e.mock.upstream.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn attachments_union_is_ordered_by_priority_then_name_without_duplicates() {
    let e = env().await;
    let team = tokomotive::db::create_team(&e.st.db, "Research")
        .await
        .unwrap();
    sqlx::query("UPDATE users SET team_id=?")
        .bind(team)
        .execute(&e.st.db)
        .await
        .unwrap();
    let (z, _) = e.create("z-first", &["pre_request"]).await;
    let (a, _) = e.create("a-second", &["pre_request"]).await;
    let (b, _) = e.create("b-third", &["pre_request"]).await;
    let (c, _) = e.create("c-fourth", &["pre_request"]).await;
    let (unused, _) = e.create("unused", &["pre_request"]).await;
    sqlx::query("UPDATE hooks SET priority=-1 WHERE id=?")
        .bind(z)
        .execute(&e.st.db)
        .await
        .unwrap();
    e.attach(z, &[("global", "on")]).await;
    e.attach(z, &[("route_name", "chat")]).await;
    e.attach(a, &[("route_name", "chat")]).await;
    e.attach(b, &[("team_name", "Research")]).await;
    let key: i64 = sqlx::query_scalar("SELECT id FROM virtual_keys LIMIT 1")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    e.attach(c, &[("key_id", &key.to_string())]).await;
    let other = tokomotive::db::create_team(&e.st.db, "Other")
        .await
        .unwrap();
    assert!(other > 0);
    e.attach(unused, &[("team_name", "Other")]).await;
    e.send("scopes", false).await.bytes().await.unwrap();
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    let names: Vec<_> = calls
        .iter()
        .map(|(_, v, _)| v["hook"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["z-first", "a-second", "b-third", "c-fourth"]);
}

#[tokio::test]
async fn event_queue_is_bounded_and_drops_with_a_metric() {
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    let metrics = METRICS.get_or_init(|| tokomotive::metrics::install().unwrap());
    let e = env().await;
    let (id, _) = e.create("queue-bound", &["event"]).await;
    e.attach(id, &[("global", "on")]).await;
    e.mock.block_events.store(true, Ordering::SeqCst);
    for n in 0..72 {
        let r = e.send(&format!("queue-{n}"), true).await;
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if metrics.render().lines().any(|l| {
                l.starts_with("llm_hook_events_dropped_total{")
                    && l.contains("queue-bound")
                    && l.rsplit_once(' ')
                        .and_then(|(_, n)| n.parse::<u64>().ok())
                        .is_some_and(|n| n >= 4)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(e.mock.calls.lock().unwrap().len(), 4);
    e.mock.block_events.store(false, Ordering::SeqCst);
    e.mock.event_gate.notify_waiters();
    e.st.flush().await;
    assert!(e.mock.calls.lock().unwrap().len() <= 68);
}

#[tokio::test]
async fn include_bodies_is_explicit_and_contains_the_final_answer() {
    let e = env().await;
    let (id, _) = e.create("bodies", &["event"]).await;
    e.attach(id, &[("global", "on")]).await;
    sqlx::query("UPDATE hooks SET include_bodies=1 WHERE id=?")
        .bind(id)
        .execute(&e.st.db)
        .await
        .unwrap();
    e.send("explicit prompt", false)
        .await
        .bytes()
        .await
        .unwrap();
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(
        calls[0].1["data"]["request"]["messages"][0]["content"],
        "explicit prompt"
    );
    assert_eq!(
        calls[0].1["data"]["response"]["choices"][0]["message"]["content"],
        "hello"
    );
}

#[tokio::test]
async fn permitted_model_change_is_resolved_again_without_a_second_hook_or_admission() {
    let e = env().await;
    let (id, _) = e.create("switch-model", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    let mut settings = (*e.st.settings()).clone();
    settings.mode = "full".into();
    tokomotive::settings::put(
        &e.st.db,
        "runtime_settings",
        &serde_json::to_string(&settings).unwrap(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE virtual_keys SET rpm_limit=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    e.st.reload_settings().await.unwrap();
    sqlx::query("UPDATE virtual_keys SET allowed_models='[\"chat\",\"p/m\"]'")
        .execute(&e.st.db)
        .await
        .unwrap();
    *e.mock.reply.lock().unwrap() =
        json!({"decision":"modify","patch":{"model":"p/m","key_id":999,"team_id":999}});
    let r = e.send("switch", false).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert_eq!(e.mock.calls.lock().unwrap().len(), 1);
    assert_eq!(e.mock.upstream.lock().unwrap()[0]["model"], "m");
    let rows = tokomotive::db::recent_ledger(&e.st.db, Default::default(), 1)
        .await
        .unwrap();
    let user: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name='alice'")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(rows[0].user_id, user);
    assert_eq!(rows[0].provider, "p");
    assert_eq!(e.send("second", false).await.status(), 429);
    assert_eq!(e.mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn post_response_attachment_also_disables_semantic_storage() {
    let e = env().await;
    sqlx::query("UPDATE routes SET config_json=json_set(config_json,'$.cache.semantic',json(?))")
        .bind(json!({"enabled":true,"embedding_model":"p/m","scope":"key"}).to_string())
        .execute(&e.st.db)
        .await
        .unwrap();
    e.st.reload_settings().await.unwrap();
    let (id, _) = e.create("no-semantic-store", &["post_response"]).await;
    e.attach(id, &[("route_name", "chat")]).await;
    e.send("one", false).await.bytes().await.unwrap();
    e.st.flush().await;
    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM response_cache")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(entries, 0);
    let embeddings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ledger WHERE semantic_cache_embed=1")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
    assert_eq!(
        embeddings, 1,
        "exercise semantic lookup before checking storage"
    );
}

#[tokio::test]
async fn management_samples_pagination_edit_delete_and_audit_use_real_http() {
    let e = env().await;
    let (id, secret) = e
        .create("a-sample", &["pre_request", "post_response", "event"])
        .await;
    e.create("b-page", &["pre_request"]).await;
    let html = e
        .client()
        .get(format!("{}/admin/hooks?limit=1", e.url))
        .header("cookie", "tokomotive_session=hooks-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("a-sample"));
    assert!(!html.contains("b-page"));
    assert!(html.contains("rel=\"next\""));
    assert!(
        html.contains("data-plain"),
        "secret forms must not become htmx HTML swaps"
    );
    for stage in ["pre_request", "post_response", "event"] {
        let r = e
            .admin(&format!("/admin/hooks/{id}/test"), &[("stage", stage)])
            .await;
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["status"], 200);
        assert_eq!(v["decision"]["decision"], "continue");
    }
    assert!(e.mock.upstream.lock().unwrap().is_empty());
    let r = e
        .admin(
            "/admin/hooks",
            &[
                ("id", &id.to_string()),
                ("name", "edited"),
                ("url", &e.hook),
                ("pre_request", "on"),
                ("enabled", "on"),
                ("allow_private", "on"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    let encrypted: String = sqlx::query_scalar("SELECT secret FROM hooks WHERE id=?")
        .bind(id)
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(
        e.st.secret_key.decrypt(&encrypted).unwrap(),
        secret,
        "edit must not rotate the secret"
    );
    assert_eq!(
        e.admin(&format!("/admin/hooks/{id}/delete"), &[])
            .await
            .status(),
        303
    );
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM admin_audit WHERE target_type='hooks'")
            .fetch_all(&e.st.db)
            .await
            .unwrap();
    assert!(actions.iter().any(|a| a.ends_with("/delete")));
    assert!(actions.iter().any(|a| a == "POST /admin/hooks"));
}

#[tokio::test]
async fn rate_budget_and_request_guardrails_precede_hook_delivery() {
    let e = env().await;
    let (id, _) = e.create("admission", &["pre_request"]).await;
    e.attach(id, &[("global", "on")]).await;
    let mut settings = (*e.st.settings()).clone();
    settings.mode = "full".into();
    tokomotive::settings::put(
        &e.st.db,
        "runtime_settings",
        &serde_json::to_string(&settings).unwrap(),
    )
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    sqlx::query("UPDATE virtual_keys SET rpm_limit=0")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(e.send("rate", false).await.status(), 429);
    assert!(e.mock.calls.lock().unwrap().is_empty());
    sqlx::query("UPDATE virtual_keys SET rpm_limit=NULL")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET budget_micros=0")
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(e.send("budget", false).await.status(), 402);
    assert!(e.mock.calls.lock().unwrap().is_empty());
    sqlx::query("UPDATE users SET budget_micros=NULL")
        .execute(&e.st.db)
        .await
        .unwrap();
    let rules = json!([{"name":"email","kind":{"type":"detector","detector":"email"},"action":"block","direction":"request"}]);
    let set = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('block-email',?)")
        .bind(rules.to_string())
        .execute(&e.st.db)
        .await
        .unwrap()
        .last_insert_rowid();
    sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
        .bind(set)
        .execute(&e.st.db)
        .await
        .unwrap();
    assert_eq!(e.send("alice@example.com", false).await.status(), 400);
    assert!(e.mock.calls.lock().unwrap().is_empty());
    assert!(e.mock.upstream.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guardrail_rejection_still_delivers_the_final_ledger_event() {
    let e = env().await;
    let (id, _) = e.create("denied-event", &["pre_request", "event"]).await;
    e.attach(id, &[("global", "on")]).await;
    let rules = json!([{"name":"email","kind":{"type":"detector","detector":"email"},"action":"block","direction":"request"}]);
    let set = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('block',?)")
        .bind(rules.to_string())
        .execute(&e.st.db)
        .await
        .unwrap()
        .last_insert_rowid();
    sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
        .bind(set)
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e.send("alice@example.com", false).await;
    assert_eq!(r.status(), 400);
    r.bytes().await.unwrap();
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["stage"], "event");
    assert_eq!(calls[0].1["data"]["ledger"]["status"], 400);
    assert!(!calls[0].1.to_string().contains("alice@example.com"));
}

#[tokio::test]
async fn viewer_cannot_manage_hooks_and_secrets_stay_out_of_html_audit_and_sqlite() {
    let e = env().await;
    let (id, secret) = e.create("private-signing", &["pre_request"]).await;
    let html = e
        .client()
        .get(format!("{}/admin/hooks", e.url))
        .header("cookie", "tokomotive_session=hooks-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains(&secret));
    assert!(!html.contains("name=\"secret\""));
    let r = e.admin(&format!("/admin/hooks/{id}/rotate"), &[]).await;
    assert_eq!(r.headers()["cache-control"], "no-store");
    let rotated: Value = r.json().await.unwrap();
    let new_secret = rotated["secret"].as_str().unwrap();
    assert_ne!(secret, new_secret);
    let stored: String = sqlx::query_scalar("SELECT secret FROM hooks WHERE id=?")
        .bind(id)
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_ne!(stored, new_secret);
    assert_eq!(e.st.secret_key.decrypt(&stored).unwrap(), new_secret);
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT before_json||after_json||summary FROM admin_audit WHERE target_type='hooks'",
    )
    .fetch_all(&e.st.db)
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert!(
        audit
            .iter()
            .all(|s| !s.contains(&secret) && !s.contains(new_secret))
    );
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&e.st.db)
        .await
        .unwrap();
    for file in std::fs::read_dir(e.dir.path()).unwrap() {
        let bytes = std::fs::read(file.unwrap().path()).unwrap();
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
        assert!(
            !bytes
                .windows(new_secret.len())
                .any(|w| w == new_secret.as_bytes())
        );
    }
    sqlx::query("INSERT INTO admin_accounts(username,role,password_hash) VALUES('reader','viewer','fixture')").execute(&e.st.db).await.unwrap();
    let account = tokomotive::admin::by_username(&e.st.db, "reader")
        .await
        .unwrap()
        .unwrap();
    tokomotive::admin::create_session(&e.st.db, "reader-session", 1, &account)
        .await
        .unwrap();
    for path in [
        "/admin/hooks".to_owned(),
        format!("/admin/hooks/{id}/rotate"),
        format!("/admin/hooks/{id}/delete"),
        format!("/admin/hooks/{id}/attach"),
        format!("/admin/hooks/{id}/test"),
    ] {
        let r = e
            .client()
            .post(format!("{}{path}", e.url))
            .header("origin", &e.url)
            .header("cookie", "tokomotive_session=reader-session")
            .form(&[("name", "denied")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{path}");
    }
}
