//! Session affinity through real HTTP requests and recording mock instances.
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tokomotive::{config::Config, state::AppState};

#[derive(Clone, Default)]
struct Instance {
    seen: Arc<Mutex<Vec<Value>>>,
    fail: Arc<AtomicBool>,
}
async fn answer(State(i): State<Instance>, Json(b): Json<Value>) -> axum::response::Response {
    i.seen.lock().unwrap().push(b.clone());
    if i.fail.load(Ordering::Relaxed) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if b.get("max_tokens").is_some() && b.get("system").is_some() {
        Json(json!({"id":"msg", "type":"message", "role":"assistant", "model":b["model"], "content":[{"type":"text","text":"ok"}], "stop_reason":"end_turn", "usage":{"input_tokens":1,"output_tokens":1}})).into_response()
    } else {
        Json(json!({"id":"chat", "model":b["model"], "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}], "usage":{"prompt_tokens":1,"completion_tokens":1}})).into_response()
    }
}
struct Env {
    st: AppState,
    url: String,
    http: reqwest::Client,
    instances: Vec<Instance>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn env(affinity: bool, kind: &str, router: &str) -> Env {
    static METRICS: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    let mut providers = String::new();
    let mut instances = Vec::new();
    let mut tasks = Vec::new();
    for n in 0..3 {
        let i = Instance::default();
        let app = axum::Router::new()
            .route("/v1/chat/completions", post(answer))
            .route("/v1/messages", post(answer))
            .with_state(i.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        providers.push_str(&format!(
            "\n[[providers]]\nname='p{n}'\nkind='{kind}'\nbase_url='http://{}'\napi_key='key'\n",
            listener.local_addr().unwrap()
        ));
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        instances.push(i);
    }
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "[server]\ndatabase='sqlite://{}'\nadmin_password='pw'\n[router]\nfailure_threshold=1\n{router}\n{providers}\n[[routes]]\nname='halogen'\nstrategy='round_robin'\nsession_affinity={affinity}\ndeployments=[{{provider='p0',model='model',weight=1}},{{provider='p1',model='model',weight=3}}]\n[[users]]\nname='alice'\nkeys=['sk-alice']\n[[users]]\nname='bob'\nkeys=['sk-bob']\n",
        dir.path().join("test.db").display()
    );
    let cfg: Config = toml::from_str(&text).unwrap();
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    tasks.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    Env {
        st,
        url,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        instances,
        tasks,
        _dir: dir,
    }
}
impl Env {
    async fn send(
        &self,
        anthropic: bool,
        prompt: &str,
        turns: usize,
        session: Option<&str>,
        user: &str,
    ) -> usize {
        let before: Vec<_> = self
            .instances
            .iter()
            .map(|i| i.seen.lock().unwrap().len())
            .collect();
        let mut messages = vec![json!({"role":"user","content":prompt})];
        for _ in 0..turns {
            messages.extend([
                json!({"role":"assistant","content":"ok"}),
                json!({"role":"user","content":"continue"}),
            ]);
        }
        let mut b = json!({"model":"halogen", "messages":messages});
        if anthropic {
            b["system"] = json!([{"type":"text","text":"Be helpful"}]);
            b["max_tokens"] = json!(10);
        } else {
            b["messages"]
                .as_array_mut()
                .unwrap()
                .insert(0, json!({"role":"system","content":"Be helpful"}));
        }
        let mut req = self
            .http
            .post(format!(
                "{}{path}",
                self.url,
                path = if anthropic {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                }
            ))
            .bearer_auth(format!("sk-{user}"))
            .json(&b);
        if let Some(key) = session {
            req = req.header("x-session-id", key);
        }
        let response = req.send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        self.st.flush().await;
        self.instances
            .iter()
            .enumerate()
            .rfind(|(n, i)| {
                i.seen.lock().unwrap().len() > before[*n] && !i.fail.load(Ordering::Relaxed)
            })
            .unwrap()
            .0
    }
}

#[tokio::test]
async fn growing_conversation_keeps_its_instance_for_openai_and_anthropic() {
    for (anthropic, kind) in [
        (false, "openai"),
        (true, "anthropic"),
        (false, "anthropic"),
        (true, "openai"),
    ] {
        let e = env(true, kind, "").await;
        let first = e.send(anthropic, "Hello", 0, None, "alice").await;
        for turn in 1..12 {
            assert_eq!(e.send(anthropic, "Hello", turn, None, "alice").await, first);
        }
    }
}

async fn last_session(e: &Env) -> Option<String> {
    sqlx::query_scalar("SELECT session_affinity FROM ledger ORDER BY id DESC LIMIT 1")
        .fetch_one(&e.st.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn transient_failure_preserves_pin_while_skipping_the_open_circuit() {
    let e = env(true, "openai", "").await;
    let first = e.send(false, "Failover", 0, None, "alice").await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
    e.instances[first].fail.store(true, Ordering::Relaxed);
    let second = e.send(false, "Failover", 1, None, "alice").await;
    assert_ne!(first, second);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
    let failed_attempts = e.instances[first].seen.lock().unwrap().len();
    assert_eq!(e.send(false, "Failover", 2, None, "alice").await, second);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
    assert_eq!(
        e.instances[first].seen.lock().unwrap().len(),
        failed_attempts
    );
    e.instances[first].fail.store(false, Ordering::Relaxed);
    let table = e.st.router_table();
    let candidate =
        e.st.candidates(
            &table,
            &e.st.catalog(),
            Some("halogen"),
            tokomotive::provider::Endpoint::ChatCompletions,
            false,
        )
        .into_iter()
        .find(|c| c.provider.name == format!("p{first}"))
        .unwrap();
    e.st.acquire_deployment(&candidate, &table.settings, true)
        .unwrap()
        .record(true);
    assert_eq!(
        e.send(false, "Failover", 3, None, "alice").await,
        first,
        "transient fallback does not replace the original account pin"
    );
}

impl Env {
    async fn cookie(&self) -> String {
        let r = self
            .http
            .post(format!("{}/admin/login", self.url))
            .form(&[("password", "pw")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303);
        r.headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }
}

#[tokio::test]
async fn routes_and_router_settings_expose_and_persist_affinity() {
    let e = env(true, "openai", "").await;
    e.send(false, "Visible session", 0, None, "alice").await;
    let cookie = e.cookie().await;
    let page = e
        .http
        .get(format!("{}/admin/routes", e.url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Session affinity"));
    assert!(page.contains("Active pinned sessions: 1"));
    let r = e
        .http
        .post(format!("{}/admin/settings", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("upstream_timeout_secs", "600"),
            ("session_days", "7"),
            ("failure_threshold", "1"),
            ("cooldown_secs", "30"),
            ("max_attempts", "8"),
            ("session_affinity_ttl_secs", "60"),
            ("session_affinity_capacity", "12"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let settings: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key='runtime_settings'")
            .fetch_one(&e.st.db)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(settings["router"]["session_affinity_ttl_secs"], 60);
    assert_eq!(settings["router"]["session_affinity_capacity"], 12);
    let r = e
        .http
        .post(format!("{}/admin/routes/halogen", e.url))
        .header("cookie", &cookie)
        .form(&[
            ("name", "halogen"),
            ("strategy", "round_robin"),
            ("session_affinity", "off"),
            ("deployment_provider", "p0"),
            ("deployment_model", "model"),
            ("deployment_weight", "1"),
            ("deployment_provider", "p1"),
            ("deployment_model", "model"),
            ("deployment_weight", "3"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    e.send(false, "Visible session", 1, None, "alice").await;
    assert_eq!(last_session(&e).await, None);
    assert!(e.st.active_sessions().is_empty());
    let ledger = e
        .http
        .get(format!("{}/admin/ledger", e.url))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(ledger.contains("<dt>Session</dt><dd>new</dd>"));
    assert!(!ledger.contains("Visible session"));
}

#[tokio::test]
async fn different_conversations_follow_weights() {
    let e = env(true, "openai", "").await;
    let mut heavy = 0;
    for n in 0..400 {
        heavy += usize::from(
            e.send(false, &format!("Conversation {n}"), 0, None, "alice")
                .await
                == 1,
        );
    }
    assert!(
        (260..=340).contains(&heavy),
        "weight 3 instance served {heavy}/400"
    );
    assert_eq!(e.st.active_sessions()["halogen"], 400);
}

#[tokio::test]
async fn explicit_header_overrides_changing_prefix_and_is_scoped_to_user() {
    let e = env(true, "openai", "").await;
    let first = e
        .send(false, "First prompt", 0, Some("client-session"), "alice")
        .await;
    for n in 0..15 {
        assert_eq!(
            e.send(
                false,
                &format!("Changed prefix {n}"),
                n,
                Some("client-session"),
                "alice"
            )
            .await,
            first
        );
    }
    assert_eq!(e.st.active_sessions()["halogen"], 1);
    e.send(false, "First prompt", 0, Some("client-session"), "bob")
        .await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
    assert_eq!(e.st.active_sessions()["halogen"], 2);
}

#[tokio::test]
async fn identical_automatic_prefixes_do_not_share_a_pin_between_users() {
    let e = env(true, "openai", "").await;
    e.send(false, "Identical prompt", 0, None, "alice").await;
    e.send(false, "Identical prompt", 0, None, "bob").await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
    assert_eq!(e.st.active_sessions()["halogen"], 2);
}

#[tokio::test]
async fn already_open_pinned_circuit_is_not_attempted_and_pin_is_preserved() {
    let e = env(true, "openai", "").await;
    let first = e.send(false, "Open circuit", 0, None, "alice").await;
    let table = e.st.router_table();
    let candidates = e.st.candidates(
        &table,
        &e.st.catalog(),
        Some("halogen"),
        tokomotive::provider::Endpoint::ChatCompletions,
        false,
    );
    let pinned = candidates
        .iter()
        .find(|c| c.provider.name == format!("p{first}"))
        .unwrap();
    e.st.acquire_deployment(pinned, &table.settings, false)
        .unwrap()
        .record(false);
    let attempts = e.instances[first].seen.lock().unwrap().len();
    let second = e.send(false, "Open circuit", 1, None, "alice").await;
    assert_ne!(first, second);
    assert_eq!(e.instances[first].seen.lock().unwrap().len(), attempts);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
    assert_eq!(
        e.send(false, "Open circuit", 2, None, "alice").await,
        second
    );
}

#[tokio::test]
async fn weights_change_without_moving_a_live_pin() {
    let e = env(true, "openai", "").await;
    let first = e.send(false, "Keep cache", 0, None, "alice").await;
    let mut table = (*e.st.router_table()).clone();
    for d in &mut table.routes[0].deployments {
        d.weight = if d.provider == format!("p{first}") {
            1
        } else {
            1_000_000
        };
    }
    e.st.set_router_table(table);
    assert_eq!(e.send(false, "Keep cache", 1, None, "alice").await, first);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
}

#[tokio::test]
async fn adding_a_deployment_moves_only_its_share_of_unpinned_sessions() {
    // One entry forces eviction between passes, so this tests rendezvous,
    // not the stronger stability that a live pin provides.
    let e = env(true, "openai", "session_affinity_capacity=1").await;
    let mut before = Vec::new();
    for n in 0..200 {
        before.push(
            e.send(false, &format!("Stable membership {n}"), 0, None, "alice")
                .await,
        );
    }
    let mut table = (*e.st.router_table()).clone();
    table.routes[0]
        .deployments
        .push(tokomotive::routing::Deployment {
            provider: "p2".into(),
            model: "model".into(),
            weight: 1,
        });
    e.st.set_router_table(table);
    let mut moved = 0;
    for (n, old) in before.into_iter().enumerate() {
        let next = e
            .send(false, &format!("Stable membership {n}"), 1, None, "alice")
            .await;
        if next != old {
            assert_eq!(next, 2);
            moved += 1;
        }
    }
    assert!((15..=70).contains(&moved), "moved {moved}/200 sessions");
}

#[tokio::test]
async fn idle_ttl_expires_the_pin() {
    let e = env(true, "openai", "session_affinity_ttl_secs=1").await;
    e.send(false, "Expire", 0, None, "alice").await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(e.st.active_sessions().is_empty());
    e.send(false, "Expire", 1, None, "alice").await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
}

#[tokio::test]
async fn capacity_evicts_the_least_recently_used_pin() {
    let e = env(true, "openai", "session_affinity_capacity=2").await;
    for prompt in ["A", "B", "A", "C", "A"] {
        e.send(false, prompt, 0, None, "alice").await;
    }
    assert_eq!(e.st.active_sessions()["halogen"], 2);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
    e.send(false, "B", 0, None, "alice").await;
    assert_eq!(last_session(&e).await.as_deref(), Some("new"));
    assert_eq!(e.st.active_sessions()["halogen"], 2);
}

#[tokio::test]
async fn affinity_off_preserves_round_robin_and_one_deployment_ignores_affinity() {
    let e = env(false, "openai", "").await;
    let mut choices = Vec::new();
    for turn in 0..8 {
        choices.push(
            e.send(false, "Same prompt", turn, Some("same"), "alice")
                .await,
        );
    }
    assert_eq!(choices, vec![0, 1, 0, 1, 0, 1, 0, 1]);
    assert!(e.st.active_sessions().is_empty());
    assert_eq!(last_session(&e).await, None);
    let mut table = (*e.st.router_table()).clone();
    table.routes[0].session_affinity = true;
    table.routes[0].deployments.truncate(1);
    e.st.set_router_table(table);
    e.send(false, "Same prompt", 10, None, "alice").await;
    assert_eq!(last_session(&e).await, None);
}

#[tokio::test]
async fn successful_fallback_is_pinned_for_the_next_turn() {
    let e = env(true, "openai", "").await;
    let mut table = (*e.st.router_table()).clone();
    table.routes[0].fallbacks.push("backup".into());
    table.routes.push(tokomotive::routing::Route {
        cache: Default::default(),
        name: "backup".into(),
        strategy: tokomotive::config::Strategy::Priority,
        session_affinity: false,
        deployments: vec![tokomotive::routing::Deployment {
            provider: "p2".into(),
            model: "model".into(),
            weight: 1,
        }],
        fallbacks: vec![],
    });
    e.st.set_router_table(table);
    e.instances[0].fail.store(true, Ordering::Relaxed);
    e.instances[1].fail.store(true, Ordering::Relaxed);
    assert_eq!(e.send(false, "Fallback", 0, None, "alice").await, 2);
    assert_eq!(last_session(&e).await.as_deref(), Some("re-pinned"));
    assert_eq!(e.send(false, "Fallback", 1, None, "alice").await, 2);
    assert_eq!(last_session(&e).await.as_deref(), Some("pinned"));
}

#[tokio::test]
async fn a_pin_cannot_bypass_provider_access() {
    let e = env(true, "openai", "").await;
    let first = e.send(false, "Permissions", 0, None, "alice").await;
    let auth = tokomotive::db::lookup_key(&e.st.db, "sk-alice")
        .await
        .unwrap()
        .unwrap();
    let other = 1 - first;
    tokomotive::db::update_key_providers(&e.st.db, auth.key_id, &[format!("p{other}")])
        .await
        .unwrap();
    let attempts = e.instances[first].seen.lock().unwrap().len();
    assert_eq!(e.send(false, "Permissions", 1, None, "alice").await, other);
    assert_eq!(e.instances[first].seen.lock().unwrap().len(), attempts);
    assert_eq!(last_session(&e).await.as_deref(), Some("re-pinned"));
}

#[tokio::test]
async fn failed_requests_keep_the_attempt_limit_and_do_not_create_pins() {
    let e = env(true, "openai", "max_attempts=1").await;
    for i in &e.instances {
        i.fail.store(true, Ordering::Relaxed);
    }
    let r = e
        .http
        .post(format!("{}/v1/chat/completions", e.url))
        .bearer_auth("sk-alice")
        .json(&json!({"model":"halogen","messages":[{"role":"user","content":"Failure"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    r.bytes().await.unwrap();
    e.st.flush().await;
    assert_eq!(
        e.instances
            .iter()
            .map(|i| i.seen.lock().unwrap().len())
            .sum::<usize>(),
        1
    );
    assert!(e.st.active_sessions().is_empty());
    assert_eq!(last_session(&e).await, None);
}

#[tokio::test]
async fn metrics_show_hit_new_and_repinned_without_session_keys() {
    let e = env(true, "openai", "").await;
    let first = e
        .send(
            false,
            "Metric prompt",
            0,
            Some("private-session-key"),
            "alice",
        )
        .await;
    e.send(
        false,
        "Metric prompt",
        1,
        Some("private-session-key"),
        "alice",
    )
    .await;
    tokomotive::quota::record(
        &e.st.db,
        &format!("p{first}"),
        &tokomotive::quota::key_id("key"),
        401,
        &axum::http::HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    e.send(
        false,
        "Metric prompt",
        2,
        Some("private-session-key"),
        "alice",
    )
    .await;
    let text = e
        .http
        .get(format!("{}/metrics", e.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for result in ["hit", "new", "repinned"] {
        assert!(
            text.lines()
                .any(|line| line.starts_with("llm_session_affinity_total{")
                    && line.contains("route=\"halogen\"")
                    && line.contains(&format!("result=\"{result}\"")))
        );
    }
    assert!(!text.contains("private-session-key") && !text.contains("Metric prompt"));
}
