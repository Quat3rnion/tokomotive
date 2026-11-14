//! Scheduling through the public HTTP router and quota observation API.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokomotive::{config::Config, quota, state::AppState};

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<String>>>,
    status: Arc<Mutex<u16>>,
    retry: Arc<Mutex<Option<String>>>,
}
async fn answer(State(up): State<Upstream>, h: HeaderMap) -> axum::response::Response {
    up.seen
        .lock()
        .unwrap()
        .push(h["authorization"].to_str().unwrap().into());
    let status = *up.status.lock().unwrap();
    if status != 0 {
        let mut response = StatusCode::from_u16(status).unwrap().into_response();
        if let Some(retry) = up.retry.lock().unwrap().as_ref() {
            response
                .headers_mut()
                .insert("retry-after", retry.parse().unwrap());
        }
        return response;
    }
    Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})).into_response()
}
struct Env {
    st: AppState,
    url: String,
    up: Upstream,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn env(extra: &str) -> Env {
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    let up = Upstream::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(answer))
        .with_state(up.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database='sqlite::memory:'
mode='simple'
[[providers]]
name='pool'
kind='openai'
base_url='{base}'
api_keys=['a','b','c']
{extra}
[[users]]
name='alice'
keys=['sk-alice']
"#
    ))
    .unwrap();
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
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        st,
        url,
        up,
        tasks: vec![task, proxy],
    }
}
impl Env {
    async fn send(&self, path: &str, model: &str, session: Option<&str>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(format!("{}{path}", self.url))
            .bearer_auth("sk-alice")
            .json(
                &json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_tokens":10}),
            );
        if let Some(s) = session {
            req = req.header("x-session-id", s);
        }
        req.send().await.unwrap()
    }
    async fn used(&self, key: &str, percent: u8, reset: i64) {
        let mut h = HeaderMap::new();
        h.insert(
            "x-codex-primary-used-percent",
            percent.to_string().parse().unwrap(),
        );
        h.insert(
            "x-codex-primary-reset-at",
            reset.to_string().parse().unwrap(),
        );
        quota::record(
            &self.st.db,
            "pool",
            &quota::key_id(key),
            200,
            &h,
            chrono::Utc::now().timestamp(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn pool_prefers_most_remaining_quota_then_reset_and_existing_order() {
    let e = env("key_strategy='quota'").await;
    let now = chrono::Utc::now().timestamp();
    e.used("a", 80, now + 500).await;
    e.used("b", 10, now + 200).await;
    e.used("c", 40, now + 100).await;
    for expected in ["Bearer b", "Bearer c", "Bearer a"] {
        assert_eq!(
            e.send("/v1/chat/completions", "pool/model", None)
                .await
                .status(),
            200
        );
        assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), expected);
        let key = expected.strip_prefix("Bearer ").unwrap();
        e.used(key, 100, now + 1000).await;
    }
}

#[tokio::test]
async fn exhausted_pool_returns_429_and_retry_after_in_both_api_formats_then_recovers() {
    let e = env("key_strategy='quota'").await;
    let now = chrono::Utc::now().timestamp();
    for (key, seconds) in [("a", 600), ("b", 300), ("c", 900)] {
        e.used(key, 98, now + seconds).await;
    }
    for path in ["/v1/chat/completions", "/v1/messages"] {
        let r = e.send(path, "pool/model", None).await;
        assert_eq!(r.status(), 429);
        let retry: i64 = r.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((295..=300).contains(&retry));
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        if path == "/v1/messages" {
            assert_eq!(body["type"], "error");
        }
    }
    assert!(e.up.seen.lock().unwrap().is_empty());
    // Expired observations cannot keep an account exhausted.
    e.used("b", 100, now - 1).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(*e.up.seen.lock().unwrap(), ["Bearer b"]);
}

#[tokio::test]
async fn sticky_pool_keeps_account_until_exhausted_or_invalid() {
    let e = env("key_strategy='quota'\n[[routes]]\nname='sticky'\nstrategy='quota'\nsession_affinity=true\ndeployments=[{provider='pool',model='model'}]").await;
    let now = chrono::Utc::now().timestamp();
    e.used("a", 80, now + 1000).await;
    e.used("b", 10, now + 1000).await;
    e.used("c", 90, now + 1000).await;
    assert_eq!(
        e.send("/v1/chat/completions", "sticky", Some("s"))
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
    e.used("a", 0, now + 1000).await;
    assert_eq!(
        e.send("/v1/chat/completions", "sticky", Some("s"))
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
    e.used("b", 98, now + 1000).await;
    assert_eq!(
        e.send("/v1/chat/completions", "sticky", Some("s"))
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer a");
    quota::record(
        &e.st.db,
        "pool",
        &quota::key_id("a"),
        403,
        &HeaderMap::new(),
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "sticky", Some("s"))
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer c");
}

#[tokio::test]
async fn credentials_failure_disables_all_keys_in_pool() {
    let e = env("").await;
    *e.up.status.lock().unwrap() = 401;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        401
    );
    assert_eq!(e.up.seen.lock().unwrap().len(), 3);
    *e.up.status.lock().unwrap() = 0;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        429
    );
    assert_eq!(e.up.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn late_inflight_success_or_rate_limit_cannot_reenable_invalid_credentials() {
    let e = env("").await;
    let now = chrono::Utc::now().timestamp();
    let account = quota::key_id("a");
    quota::record(&e.st.db, "pool", &account, 401, &HeaderMap::new(), now)
        .await
        .unwrap();
    for status in [200, 429, 503] {
        quota::record(
            &e.st.db,
            "pool",
            &account,
            status,
            &HeaderMap::new(),
            now + 1,
        )
        .await
        .unwrap();
        assert_eq!(
            quota::accounts(&e.st.db, Some("pool"), 10).await.unwrap()[0].state,
            "credentials_invalid"
        );
    }
    quota::recover_credentials(&e.st.db, "pool", &account)
        .await
        .unwrap();
    assert_eq!(
        quota::accounts(&e.st.db, Some("pool"), 10).await.unwrap()[0].state,
        "ok"
    );
}

#[tokio::test]
async fn unrelated_health_observation_does_not_extend_an_unknown_window_cooldown() {
    let e = env("key_strategy='quota'").await;
    let now = chrono::Utc::now().timestamp();
    e.used("b", 100, now + 600).await;
    e.used("c", 100, now + 600).await;
    let mut h = HeaderMap::new();
    h.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    let account = quota::key_id("a");
    quota::record(&e.st.db, "pool", &account, 200, &h, now - 31)
        .await
        .unwrap();
    quota::record(&e.st.db, "pool", &account, 200, &HeaderMap::new(), now)
        .await
        .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer a");
}

#[tokio::test]
async fn quota_deployments_rank_by_their_best_account() {
    let e = env("key_strategy='quota'\n[[routes]]\nname='ranked'\nstrategy='quota'\ndeployments=[{provider='pool',model='first'},{provider='other',model='second'}]\n[[providers]]\nname='other'\nkind='openai'\nbase_url='http://127.0.0.1:1'\napi_key='other-key'").await;
    // Point the second Deployment at the same recording upstream after setup.
    let mut table = (*e.st.router_table()).clone();
    let base = table
        .providers
        .iter()
        .find(|p| p.name == "pool")
        .unwrap()
        .base_url
        .clone();
    table
        .providers
        .iter_mut()
        .find(|p| p.name == "other")
        .unwrap()
        .base_url = base;
    e.st.set_router_table(table);
    let now = chrono::Utc::now().timestamp();
    for key in ["a", "b", "c"] {
        e.used(key, 80, now + 1000).await;
    }
    let mut h = HeaderMap::new();
    h.insert("x-codex-primary-used-percent", "10".parse().unwrap());
    quota::record(&e.st.db, "other", &quota::key_id("other-key"), 200, &h, now)
        .await
        .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "ranked", None)
            .await
            .status(),
        200
    );
    assert_eq!(
        e.up.seen.lock().unwrap().last().unwrap(),
        "Bearer other-key"
    );
}

#[tokio::test]
async fn health_alerts_only_emit_transitions_and_dedupe_flapping_for_ten_minutes() {
    let e = env("").await;
    let key = quota::key_id("a");
    let h = HeaderMap::new();
    for (status, time, expected) in [
        (200, 1000, None),
        (429, 1001, Some("rate_limited")),
        (429, 1002, None),
        (429, 1003, None),
        (200, 1004, Some("recovered")),
        (429, 1005, None),
        (200, 1006, None),
        (429, 1601, Some("rate_limited")),
        (401, 1602, Some("credentials_invalid")),
    ] {
        quota::record(&e.st.db, "pool", &key, status, &h, time)
            .await
            .unwrap();
        let events = quota::take_health_events(&e.st.db).await.unwrap();
        assert_eq!(events.len(), usize::from(expected.is_some()));
        if let Some(state) = expected {
            assert_eq!(events[0].kind, format!("provider.{state}"));
        }
    }
}

#[tokio::test]
async fn operator_can_reenable_an_account_and_select_quota_in_the_ui() {
    let e=env("[[routes]]\nname='ranked'\nstrategy='quota'\ndeployments=[{provider='pool',model='model'}]").await;
    tokomotive::settings::put(&e.st.db, "admin_password_hash", "configured")
        .await
        .unwrap();
    tokomotive::db::create_session(&e.st.db, "operator-session", 1)
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let account = quota::key_id("a");
    quota::record(
        &e.st.db,
        "pool",
        &account,
        401,
        &HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    for path in ["/admin/providers", "/admin/routes"] {
        let html = http
            .get(format!("{}{path}", e.url))
            .header("cookie", "tokomotive_session=operator-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(html.contains("Disabled: credentials invalid"), "{path}");
        assert!(html.contains("value=\"quota\""), "{path}");
    }
    let url = format!("{}/admin/providers/pool/accounts/{account}/reenable", e.url);
    assert_eq!(
        http.post(&url).send().await.unwrap().status(),
        303,
        "authentication is required"
    );
    assert_eq!(
        http.post(&url)
            .header("cookie", "tokomotive_session=operator-session")
            .header("origin", "https://other.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let r = http
        .post(&url)
        .header("cookie", "tokomotive_session=operator-session")
        .header("origin", &e.url)
        .header("hx-request", "true")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert!(r.headers().contains_key("hx-redirect"));
    assert_eq!(
        quota::accounts(&e.st.db, Some("pool"), 10).await.unwrap()[0].state,
        "ok"
    );
    let r = http
        .post(format!("{}/admin/providers/pool/key-strategy", e.url))
        .header("cookie", "tokomotive_session=operator-session")
        .form(&[("strategy", "quota")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(
        e.st.router_table().providers[0].key_strategy,
        tokomotive::config::KeyStrategy::Quota
    );
}

#[tokio::test]
async fn known_rate_limit_reset_survives_circuit_cooldown_and_restart_of_schedule() {
    let e = env("").await;
    *e.up.status.lock().unwrap() = 429;
    *e.up.retry.lock().unwrap() = Some("600".into());
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        429
    );
    assert_eq!(e.up.seen.lock().unwrap().len(), 3);
    *e.up.status.lock().unwrap() = 0;
    let r = e.send("/v1/chat/completions", "pool/model", None).await;
    assert_eq!(r.status(), 429);
    assert!(
        r.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 595
    );
    assert_eq!(e.up.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn unknown_quota_ranks_in_middle_then_earliest_reset_then_original_order() {
    let e = env("key_strategy='quota'").await;
    let now = chrono::Utc::now().timestamp();
    e.used("a", 80, now + 300).await;
    e.used("b", 10, now + 300).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
    e.used("b", 98, now + 300).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer c");
    e.used("a", 50, now + 200).await;
    e.used("b", 50, now + 100).await;
    e.used("c", 50, now + 100).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
}

#[tokio::test]
async fn default_key_pool_still_rotates_despite_unequal_quotas() {
    let e = env("").await;
    let now = chrono::Utc::now().timestamp();
    e.used("a", 90, now + 1000).await;
    e.used("b", 10, now + 1000).await;
    for _ in 0..2 {
        for key in ["a", "b", "c"] {
            assert_eq!(
                e.send("/v1/chat/completions", "pool/model", None)
                    .await
                    .status(),
                200
            );
            assert_eq!(
                e.up.seen.lock().unwrap().last().unwrap(),
                &format!("Bearer {key}")
            );
        }
    }
}

#[tokio::test]
async fn pool_uses_its_most_constrained_window_and_configured_exhaustion_threshold() {
    let e = env("key_strategy='quota'\n[router]\nquota_exhausted_percent=80").await;
    let now = chrono::Utc::now().timestamp();
    e.used("a", 10, now + 600).await;
    e.used("b", 50, now + 600).await;
    e.used("c", 60, now + 600).await;
    let mut h = HeaderMap::new();
    h.insert("x-codex-secondary-used-percent", "90".parse().unwrap());
    h.insert(
        "x-codex-secondary-reset-at",
        (now + 900).to_string().parse().unwrap(),
    );
    quota::record(&e.st.db, "pool", &quota::key_id("a"), 200, &h, now)
        .await
        .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
    e.used("b", 80, now + 600).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer c");
}

#[tokio::test]
async fn route_display_does_not_consume_round_robin_key_turns() {
    let e=env("[[routes]]\nname='rr'\nstrategy='round_robin'\ndeployments=[{provider='pool',model='model'}]").await;
    tokomotive::settings::put(&e.st.db, "admin_password_hash", "configured")
        .await
        .unwrap();
    tokomotive::db::create_session(&e.st.db, "operator-session", 1)
        .await
        .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "rr", None).await.status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer a");
    reqwest::Client::new()
        .get(format!("{}/admin/routes", e.url))
        .header("cookie", "tokomotive_session=operator-session")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        e.send("/v1/chat/completions", "rr", None).await.status(),
        200
    );
    assert_eq!(e.up.seen.lock().unwrap().last().unwrap(), "Bearer b");
    quota::record(
        &e.st.db,
        "pool",
        &quota::key_id("a"),
        401,
        &HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}/admin/providers/pool/keys", e.url))
        .header("cookie", "tokomotive_session=operator-session")
        .form(&[("api_key", "a")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let accounts = quota::accounts(&e.st.db, Some("pool"), 10).await.unwrap();
    assert_eq!(
        accounts
            .iter()
            .find(|a| a.key == quota::key_id("a"))
            .unwrap()
            .state,
        "ok",
        "a key edit permits credential recovery"
    );
}

#[tokio::test]
async fn proxy_delivers_health_transition_webhooks_and_exports_skip_reasons() {
    let e = env("").await;
    let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let received = events.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/hook",
        post(move |Json(event): Json<serde_json::Value>| {
            let received = received.clone();
            async move {
                received.lock().unwrap().push(event);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let hook = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokomotive::alerts::save(&e.st, None, &url, "generic", true)
        .await
        .unwrap();
    *e.up.status.lock().unwrap() = 401;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        401
    );
    e.st.flush().await;
    assert_eq!(events.lock().unwrap().len(), 3);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|v| v["kind"] == "provider.credentials_invalid")
    );
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        429
    );
    e.st.flush().await;
    assert_eq!(
        events.lock().unwrap().len(),
        3,
        "skips do not repeat health alerts"
    );
    quota::reenable(&e.st, "pool", &quota::key_id("a"))
        .await
        .unwrap();
    e.st.flush().await;
    assert_eq!(
        events.lock().unwrap().last().unwrap()["kind"],
        "provider.recovered"
    );
    e.used("a", 100, chrono::Utc::now().timestamp() + 600).await;
    assert_eq!(
        e.send("/v1/chat/completions", "pool/model", None)
            .await
            .status(),
        429
    );
    let metrics = reqwest::Client::new()
        .get(format!("{}/metrics", e.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for reason in ["credentials_invalid", "rate_limited"] {
        assert!(
            metrics
                .lines()
                .any(|line| line.starts_with("tokomotive_account_skips_total{")
                    && line.contains(&format!("reason=\"{reason}\""))),
            "{metrics}"
        );
    }
    hook.abort();
}
