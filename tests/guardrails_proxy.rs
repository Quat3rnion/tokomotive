use axum::{Json, extract::State, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokomotive::{
    config::Config,
    guardrails::{Action, Direction, Kind, Rule},
    state::AppState,
};

struct Env {
    st: AppState,
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn answer(
    State(seen): State<Arc<Mutex<Vec<Value>>>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Json(body): Json<Value>,
) -> axum::response::Response {
    seen.lock().unwrap().push(body.clone());
    if body["messages"][0]["content"] == "invalid-response" {
        return ([("content-type", "application/json")], "alice@example.com").into_response();
    }
    if body["messages"][0]["content"] == "invalid-stream" {
        return (
            [("content-type", "text/event-stream")],
            "data: alice@example.com\n\n",
        )
            .into_response();
    }
    if body["stream"] == true && body["messages"][0]["content"] == "window" {
        let parts = [
            format!("{} ", "z".repeat(17000)),
            "alice@".into(),
            format!("example.com{}", " ".repeat(12000)),
            " ".repeat(4000),
        ]
        .into_iter()
        .map(|text| {
            Ok::<_, std::io::Error>(format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":text}}]})
            ))
        });
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(futures_util::stream::iter(parts)),
        )
            .into_response();
    }
    if body["stream"] == true {
        let parts = if uri.path().ends_with("messages") {
            vec!["event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n".to_owned(), "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"alice@\"}}\n\n".to_owned(), "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"example.com\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned()]
        } else {
            vec![format!("data: {}\n\n", json!({"id":"c","model":"m","choices":[{"index":0,"delta":{"content":"alice@"}}]})), format!("data: {}\n\n", json!({"id":"c","model":"m","choices":[{"index":0,"delta":{"content":"example.com"}}]})), "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n".into()]
        };
        let stream = futures_util::stream::iter(parts.into_iter().map(Ok::<_, std::io::Error>));
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }
    if uri.path().ends_with("messages") {
        return Json(json!({"id":"m","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"alice@example.com"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}})).into_response();
    }
    Json(json!({"id":"c","model":"m","choices":[{"message":{"role":"assistant","content":"alice@example.com"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3}})).into_response()
}
async fn env(kind: &str) -> Env {
    let seen = Arc::new(Mutex::new(vec![]));
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(answer))
        .route("/v1/messages", post(answer))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: Config = toml::from_str(&format!("[server]\ndatabase='sqlite::memory:'\nmode='simple'\nadmin_password='password123'\n[[providers]]\nname='p'\nkind='{kind}'\nbase_url='http://{upstream}'\napi_key='secret'\n[[routes]]\nname='chat'\ndeployments=[{{provider='p',model='m'}}]\ncache={{enabled=true,ttl_secs=60}}\n[[users]]\nname='alice'\nkeys=['sk-alice']")).unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let team = tokomotive::db::create_team(&st.db, "Research")
        .await
        .unwrap();
    sqlx::query("UPDATE users SET team_id=?")
        .bind(team)
        .execute(&st.db)
        .await
        .unwrap();
    tokomotive::db::create_session(&st.db, "guard-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        st,
        url,
        seen,
        tasks: vec![task, proxy],
    }
}
impl Env {
    async fn attach(&self, name: &str, action: Action, direction: Direction, level: &str) -> i64 {
        let rules = vec![Rule {
            name: name.into(),
            action,
            direction,
            kind: Kind::Detector {
                detector: "email".into(),
            },
        }];
        let id = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES(?,?)")
            .bind(name)
            .bind(serde_json::to_string(&rules).unwrap())
            .execute(&self.st.db)
            .await
            .unwrap()
            .last_insert_rowid();
        let sql = match level {
            "route" => "INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')",
            "team" => {
                "INSERT INTO guardrail_attachments(set_id,team_id) SELECT ?,id FROM teams WHERE name='Research'"
            }
            "key" => {
                "INSERT INTO guardrail_attachments(set_id,key_id) SELECT ?,id FROM virtual_keys LIMIT 1"
            }
            _ => unreachable!(),
        };
        sqlx::query(sql)
            .bind(id)
            .execute(&self.st.db)
            .await
            .unwrap();
        id
    }
    async fn send(&self, path: &str, content: &str, stream: bool) -> reqwest::Response {
        reqwest::Client::new().post(format!("{}/v1/{path}", self.url)).bearer_auth("sk-alice").json(&json!({"model":"chat","messages":[{"role":"user","content":content}],"max_tokens":100,"stream":stream})).send().await.unwrap()
    }
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }
}

#[tokio::test]
async fn request_redaction_reaches_upstream_and_capture_in_simple_mode() {
    let e = env("openai").await;
    e.attach("email", Action::Redact, Direction::Request, "route")
        .await;
    sqlx::query("UPDATE virtual_keys SET capture_bodies=1")
        .execute(&e.st.db)
        .await
        .unwrap();
    let r = e.send("chat/completions", "alice@example.com", false).await;
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert_eq!(
        e.seen.lock().unwrap()[0]["messages"][0]["content"],
        "[REDACTED:email]"
    );
    let id: i64 = sqlx::query_scalar("SELECT max(id) FROM ledger")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    let bodies = tokomotive::observability::bodies(&e.st.db, &e.st.secret_key, id)
        .await
        .unwrap()
        .unwrap();
    assert!(!bodies.request.contains("alice@example.com"));
    assert!(bodies.request.contains("[REDACTED:email]"));
    assert_eq!(
        tokomotive::db::recent_ledger(&e.st.db, Default::default(), 1)
            .await
            .unwrap()[0]
            .guardrail_rules,
        "email"
    );
}

#[tokio::test]
async fn resolved_route_controls_apply_to_case_insensitive_and_catch_all_names() {
    let e = env("openai").await;
    e.attach("email", Action::Block, Direction::Request, "route")
        .await;
    let mut settings = (*e.st.settings()).clone();
    settings.router.unknown_model_route = Some("chat".into());
    tokomotive::settings::put(
        &e.st.db,
        "runtime_settings",
        &serde_json::to_string(&settings).unwrap(),
    )
    .await
    .unwrap();
    e.st.reload_settings().await.unwrap();
    for model in ["CHAT", "unknown"] {
        let r = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", e.url))
            .bearer_auth("sk-alice")
            .json(
                &json!({"model":model,"messages":[{"role":"user","content":"alice@example.com"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{model}");
        assert_eq!(
            r.json::<Value>().await.unwrap()["error"]["code"],
            "guardrail_blocked"
        );
    }
    assert!(e.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn attachment_union_block_wins_and_formats_never_echo_match() {
    for (kind, path) in [("openai", "chat/completions"), ("anthropic", "messages")] {
        let e = env(kind).await;
        e.attach("route-log", Action::Log, Direction::Request, "route")
            .await;
        e.attach("team-redact", Action::Redact, Direction::Request, "team")
            .await;
        e.attach("key-block", Action::Block, Direction::Request, "key")
            .await;
        let r = e.send(path, "alice@example.com", false).await;
        assert_eq!(r.status(), 400);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "guardrail_blocked");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("key-block")
        );
        assert!(!v.to_string().contains("alice@example.com"));
        if kind == "anthropic" {
            assert_eq!(v["type"], "error");
        }
        e.st.flush().await;
        let flags: String = sqlx::query_scalar("SELECT rule_names FROM guardrail_events")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
        assert_eq!(flags, "key-block, route-log, team-redact");
        assert!(e.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn request_checks_precede_cache_and_response_checks_apply_to_hits() {
    let e = env("openai").await;
    let first = e.send("chat/completions", "hello", false).await;
    assert_eq!(first.status(), 200);
    first.bytes().await.unwrap();
    e.st.flush().await;
    e.attach("email", Action::Redact, Direction::Response, "route")
        .await;
    let hit = e.send("chat/completions", "hello", false).await;
    assert_eq!(hit.headers()["x-tokomotive-cache"], "hit");
    assert_eq!(
        hit.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "[REDACTED:email]"
    );
    assert_eq!(e.seen.lock().unwrap().len(), 1);
    let id = e
        .attach("block", Action::Block, Direction::Request, "key")
        .await;
    sqlx::query("UPDATE guardrail_sets SET rules=? WHERE id=?").bind(r#"[{"name":"hello-block","kind":{"type":"keywords","words":["hello"]},"action":"block","direction":"request"}]"#).bind(id).execute(&e.st.db).await.unwrap();
    assert_eq!(
        e.send("chat/completions", "hello", false).await.status(),
        400
    );
    assert_eq!(e.seen.lock().unwrap().len(), 1);
    e.st.flush().await;
}

#[tokio::test]
async fn stream_redacts_across_chunks_and_block_ends_with_client_error() {
    for (kind, path) in [("openai", "chat/completions"), ("anthropic", "messages")] {
        let e = env(kind).await;
        let id = e
            .attach("email", Action::Redact, Direction::Response, "route")
            .await;
        let r = e.send(path, "hello", true).await;
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(!text.contains("alice@"), "{text}");
        assert!(!text.contains("example.com"), "{text}");
        assert!(text.contains("[REDACTED:email]"), "{text}");
        e.st.flush().await;
        sqlx::query("UPDATE guardrail_sets SET rules=replace(rules,'redact','block') WHERE id=?")
            .bind(id)
            .execute(&e.st.db)
            .await
            .unwrap();
        let text = e.send(path, "other", true).await.text().await.unwrap();
        assert!(text.contains("guardrail_blocked"), "{text}");
        if kind == "anthropic" {
            assert!(text.contains("event: error"));
        }
        assert!(!text.contains("alice@"));
        e.st.flush().await;
        let status: i64 = sqlx::query_scalar("SELECT status FROM ledger ORDER BY id DESC LIMIT 1")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
        assert_eq!(status, 400);
        let errors: i64 = sqlx::query_scalar("SELECT sum(errors) FROM rollup_hourly")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
        assert_eq!(errors, 1);
    }
}

#[tokio::test]
async fn guardrails_admin_page_and_test_box_do_not_persist_pasted_text() {
    let e = env("openai").await;
    let id = e
        .attach("email", Action::Redact, Direction::Both, "route")
        .await;
    let c = e.client();
    let r = c
        .get(format!("{}/admin/guardrails", e.url))
        .header("cookie", "tokomotive_session=guard-session")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("Test a rule"));
    assert!(html.contains("data-kind=\"routes\""));
    assert!(html.contains("data-kind=\"teams\""));
    let r = c
        .post(format!("{}/admin/guardrails/test", e.url))
        .header("cookie", "tokomotive_session=guard-session")
        .header("origin", &e.url)
        .form(&[
            ("set_id", id.to_string()),
            ("text", "alice@example.com".into()),
            ("direction", "request".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("[REDACTED:email]"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guardrail_events")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn sliding_window_never_releases_half_of_a_redaction() {
    let e = env("openai").await;
    e.attach("email", Action::Redact, Direction::Response, "route")
        .await;
    let text = e
        .send("chat/completions", "window", true)
        .await
        .text()
        .await
        .unwrap();
    assert!(text.contains("[REDACTED:email]"));
    assert!(!text.contains("alice@"));
    assert!(!text.contains("example.com"), "retained tail leaked");
    e.st.flush().await;
}

#[tokio::test]
async fn unparseable_response_text_cannot_bypass_guardrails() {
    let e = env("openai").await;
    e.attach("email", Action::Redact, Direction::Response, "route")
        .await;
    let r = e.send("chat/completions", "invalid-response", false).await;
    assert_eq!(r.status(), 400);
    assert!(!r.text().await.unwrap().contains("alice@example.com"));
    let text = e
        .send("chat/completions", "invalid-stream", true)
        .await
        .text()
        .await
        .unwrap();
    assert!(text.contains("guardrail_blocked"));
    assert!(!text.contains("alice@example.com"));
    e.st.flush().await;
}
