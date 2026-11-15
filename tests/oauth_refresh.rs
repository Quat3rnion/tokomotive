use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokomotive::{
    db,
    oauth::{
        Flow, FlowConfig, FlowType, InferenceBase, OAuthEndpoints, OAuthManager, RefreshRule,
        TokenEncoding,
    },
    settings::SecretKey,
};

struct Fixture {
    auth: OAuthManager,
    db: db::Db,
    key: SecretKey,
    config: FlowConfig,
    clock: Arc<AtomicI64>,
    replies: Arc<Mutex<VecDeque<(StatusCode, Value, Duration)>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
    events: Arc<Mutex<Vec<Value>>>,
    max_active: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new(rotating: bool) -> Self {
        let replies = Arc::new(Mutex::new(VecDeque::<(StatusCode, Value, Duration)>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let event_sink = events.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let maximum = max_active.clone();
        let (r, q) = (replies.clone(), requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/token",
                post(move |Json(body): Json<Value>| {
                    let (r, q) = (r.clone(), q.clone());
                    let (active, maximum) = (active.clone(), maximum.clone());
                    async move {
                        maximum
                            .fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        q.lock().unwrap().push(body);
                        let (status, body, delay) = r
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("unexpected token call");
                        tokio::time::sleep(delay).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        (status, Json(body)).into_response()
                    }
                }),
            )
            .route(
                "/events",
                post(move |Json(body): Json<Value>| {
                    let sink = event_sink.clone();
                    async move {
                        sink.lock().unwrap().push(body);
                        StatusCode::NO_CONTENT
                    }
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let db = db::connect("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('sub','\"anthropic\"','https://api.anthropic.com','claude')").execute(&db).await.unwrap();
        let key = SecretKey::from_bytes([8; 32]);
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let config = FlowConfig {
            flow_type: FlowType::PkcePaste,
            authorize: format!("{base}/authorize"),
            device: None,
            token: format!("{base}/token"),
            client_id: "client".into(),
            client_secret: None,
            scopes: String::new(),
            authorize_params: vec![],
            token_encoding: TokenEncoding::Json,
            headers: vec![],
            inference_base: InferenceBase::Fixed(base.clone()),
            refresh: if rotating {
                RefreshRule::Rotating
            } else {
                RefreshRule::RefreshToken
            },
            redirect: "https://platform.claude.com/oauth/code/callback",
            instructions: "",
            device_pkce: false,
            basic_secret: false,
        };
        let c = clock.clone();
        let auth = OAuthManager::new(
            db.clone(),
            key.clone(),
            OAuthEndpoints {
                claude_resource: base,
                ..Default::default()
            },
        )
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_flow_config(Flow::Claude, config.clone());
        let f = Self {
            auth,
            db,
            key,
            config,
            clock,
            replies,
            requests,
            server,
            events,
            max_active,
        };
        f.reply(StatusCode::OK, json!({"access_token":"secret-access", "refresh_token":"secret-refresh", "expires_in":3600}), Duration::ZERO);
        let a = f.auth.start("sub", Flow::Claude).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        f.auth
            .finish("sub", Flow::Claude, &format!("code#{}", q["state"]))
            .await
            .unwrap();
        f
    }
    fn reply(&self, status: StatusCode, value: Value, delay: Duration) {
        self.replies
            .lock()
            .unwrap()
            .push_back((status, value, delay));
    }
    fn success(&self) {
        self.reply(StatusCode::OK, json!({"access_token":"new-secret", "refresh_token":"rotated-secret", "expires_in":3600}), Duration::ZERO);
    }
    fn advance_to_refresh(&self) {
        self.clock.store(4_000_000, Ordering::SeqCst);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn background_proactively_refreshes_before_expiry_with_injected_clock() {
    let f = Fixture::new(true).await;
    f.success();
    f.auth.refresh_due_credentials().await.unwrap();
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    f.advance_to_refresh();
    f.auth.refresh_due_credentials().await.unwrap();
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    let s = f.auth.status("sub", Flow::Claude).await.unwrap();
    assert_eq!(s.last_refresh, Some(4_000_000));
    assert_eq!(s.expires_at, Some(7_600_000));
    assert!(s.next_refresh_at.unwrap() > 4_000_000);
}

#[tokio::test]
async fn rotating_timeout_is_in_doubt_and_restart_retries_same_token_once() {
    let mut f = Fixture::new(true).await;
    f.auth = f
        .auth
        .clone()
        .with_refresh_timeout(Duration::from_millis(20));
    f.reply(
        StatusCode::OK,
        json!({"access_token":"lost", "refresh_token":"lost-rotation", "expires_in":3600}),
        Duration::from_secs(1),
    );
    f.advance_to_refresh();
    assert_eq!(
        f.auth.token("sub", Flow::Claude, None).await.unwrap(),
        "secret-access"
    );
    let s = f.auth.status("sub", Flow::Claude).await.unwrap();
    assert!(s.connected && s.refresh_in_doubt);
    assert!(s.last_error.is_none());
    f.clock.store(s.next_retry_at.unwrap(), Ordering::SeqCst);
    f.success();
    let c = f.clock.clone();
    let reloaded = OAuthManager::new(f.db.clone(), f.key.clone(), OAuthEndpoints::default())
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_flow_config(Flow::Claude, f.config.clone());
    assert_eq!(
        reloaded.token("sub", Flow::Claude, None).await.unwrap(),
        "new-secret"
    );
    {
        let requests = f.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1]["refresh_token"], "secret-refresh");
        assert_eq!(requests[2]["refresh_token"], "secret-refresh");
    }
    assert!(
        !reloaded
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .refresh_in_doubt
    );
}

#[tokio::test]
async fn invalid_grant_disconnects_immediately_and_cannot_replay() {
    let f = Fixture::new(true).await;
    f.reply(
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid_grant", "error_description":"secret-issuer-detail"}),
        Duration::ZERO,
    );
    f.advance_to_refresh();
    assert_eq!(
        f.auth
            .token("sub", Flow::Claude, None)
            .await
            .unwrap_err()
            .to_string(),
        "Refresh token rejected; sign in again"
    );
    assert!(!f.auth.status("sub", Flow::Claude).await.unwrap().connected);
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn in_doubt_retry_rejected_as_used_disconnects_without_a_third_attempt() {
    let mut f = Fixture::new(true).await;
    f.auth = f
        .auth
        .clone()
        .with_refresh_timeout(Duration::from_millis(20));
    f.reply(StatusCode::OK, json!({}), Duration::from_secs(1));
    f.advance_to_refresh();
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    let s = f.auth.status("sub", Flow::Claude).await.unwrap();
    f.clock.store(s.next_retry_at.unwrap(), Ordering::SeqCst);
    f.reply(StatusCode::BAD_REQUEST, json!({"error":"invalid_grant", "error_description":"Refresh token already used: secret-refresh"}), Duration::ZERO);
    assert_eq!(
        f.auth
            .token("sub", Flow::Claude, None)
            .await
            .unwrap_err()
            .to_string(),
        "Refresh token was already used; sign in again"
    );
    assert!(!f.auth.status("sub", Flow::Claude).await.unwrap().connected);
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn concurrent_refresh_requests_share_one_token_endpoint_call() {
    let f = Fixture::new(true).await;
    f.advance_to_refresh();
    f.reply(
        StatusCode::OK,
        json!({"access_token":"new-secret", "refresh_token":"new-refresh", "expires_in":3600}),
        Duration::from_millis(25),
    );
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let auth = f.auth.clone();
        tasks.spawn(async move {
            auth.token("sub", Flow::Claude, Some("secret-access"))
                .await
                .unwrap()
        });
    }
    while let Some(token) = tasks.join_next().await {
        assert_eq!(token.unwrap(), "new-secret");
    }
    assert_eq!(f.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn non_rotating_refresh_preserves_old_refresh_token_when_omitted() {
    let f = Fixture::new(false).await;
    f.advance_to_refresh();
    f.reply(
        StatusCode::OK,
        json!({"access_token":"nonrotated", "expires_in":3600}),
        Duration::ZERO,
    );
    assert_eq!(
        f.auth.token("sub", Flow::Claude, None).await.unwrap(),
        "nonrotated"
    );
    f.clock.store(7_000_000, Ordering::SeqCst);
    f.success();
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    assert_eq!(
        f.requests.lock().unwrap()[2]["refresh_token"],
        "secret-refresh"
    );
}

#[tokio::test]
async fn restart_after_crash_mid_refresh_retries_pending_instead_of_disconnecting() {
    let f = Fixture::new(true).await;
    f.advance_to_refresh();
    f.reply(StatusCode::OK, json!({}), Duration::from_secs(30));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            f.auth.token("sub", Flow::Claude, None)
        )
        .await
        .is_err()
    );
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    let s = f.auth.status("sub", Flow::Claude).await.unwrap();
    assert!(s.connected && s.refresh_in_doubt);
    assert!(s.next_retry_at.is_none());
    let c = f.clock.clone();
    let reloaded = OAuthManager::new(f.db.clone(), f.key.clone(), OAuthEndpoints::default())
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_flow_config(Flow::Claude, f.config.clone());
    f.success();
    assert_eq!(
        reloaded.token("sub", Flow::Claude, None).await.unwrap(),
        "new-secret"
    );
    assert_eq!(
        f.requests.lock().unwrap()[2]["refresh_token"],
        "secret-refresh"
    );
}

#[tokio::test]
async fn disconnect_and_recovery_alerts_are_deduplicated() {
    let f = Fixture::new(true).await;
    let cfg: tokomotive::config::Config =
        toml::from_str("[server]\ndatabase='sqlite::memory:'").unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::alerts::save(
        &st,
        None,
        &f.config.token.replace("/token", "/events"),
        "generic",
        true,
    )
    .await
    .unwrap();
    f.advance_to_refresh();
    f.reply(
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid_grant","error_description":"secret-error"}),
        Duration::ZERO,
    );
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    f.auth.emit_refresh_events(&st).await;
    f.auth.emit_refresh_events(&st).await;
    st.flush().await;
    assert_eq!(f.events.lock().unwrap().len(), 1);
    assert_eq!(f.events.lock().unwrap()[0]["kind"], "oauth_disconnected");
    f.success();
    let a = f.auth.start("sub", Flow::Claude).await.unwrap();
    let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    f.auth
        .finish("sub", Flow::Claude, &format!("code#{}", q["state"]))
        .await
        .unwrap();
    f.auth.emit_refresh_events(&st).await;
    f.auth.emit_refresh_events(&st).await;
    st.flush().await;
    let events = f.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["kind"], "oauth_recovered");
    assert!(!serde_json::to_string(&*events).unwrap().contains("secret-"));
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn background_refresh_concurrency_is_bounded_to_four() {
    let f = Fixture::new(true).await;
    for index in 1..8 {
        let name = format!("sub{index}");
        sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES(?,'\"anthropic\"','https://api.anthropic.com','claude')").bind(&name).execute(&f.db).await.unwrap();
        f.reply(
            StatusCode::OK,
            json!({"access_token":"initial", "refresh_token":"initial-refresh", "expires_in":3600}),
            Duration::ZERO,
        );
        let a = f.auth.start(&name, Flow::Claude).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        f.auth
            .finish(&name, Flow::Claude, &format!("code#{}", q["state"]))
            .await
            .unwrap();
    }
    for _ in 0..8 {
        f.reply(
            StatusCode::OK,
            json!({"access_token":"renewed", "refresh_token":"renewed-refresh", "expires_in":3600}),
            Duration::from_millis(50),
        );
    }
    f.advance_to_refresh();
    f.auth.refresh_due_credentials().await.unwrap();
    assert_eq!(f.max_active.load(Ordering::SeqCst), 4);
    assert_eq!(f.requests.lock().unwrap().len(), 16);
}

#[tokio::test]
async fn malformed_success_on_rotating_flow_disconnects_without_replay() {
    let f = Fixture::new(true).await;
    f.advance_to_refresh();
    f.reply(
        StatusCode::OK,
        json!({"unexpected":"secret-error"}),
        Duration::ZERO,
    );
    assert_eq!(
        f.auth
            .token("sub", Flow::Claude, None)
            .await
            .unwrap_err()
            .to_string(),
        "Invalid refresh response; sign in again"
    );
    assert!(!f.auth.status("sub", Flow::Claude).await.unwrap().connected);
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn second_ambiguous_rotating_attempt_stops_replay() {
    let mut f = Fixture::new(true).await;
    f.auth = f
        .auth
        .clone()
        .with_refresh_timeout(Duration::from_millis(20));
    f.advance_to_refresh();
    for _ in 0..2 {
        f.reply(StatusCode::OK, json!({}), Duration::from_secs(1));
    }
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    f.clock.store(
        f.auth
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .next_retry_at
            .unwrap(),
        Ordering::SeqCst,
    );
    assert_eq!(
        f.auth
            .token("sub", Flow::Claude, None)
            .await
            .unwrap_err()
            .to_string(),
        "Refresh token state is unknown; sign in again"
    );
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn flow_without_refresh_grant_keeps_access_token_until_actual_expiry() {
    let mut f = Fixture::new(false).await;
    f.config.refresh = RefreshRule::None;
    f.auth = f
        .auth
        .clone()
        .with_flow_config(Flow::Claude, f.config.clone());
    f.advance_to_refresh();
    assert_eq!(
        f.auth.token("sub", Flow::Claude, None).await.unwrap(),
        "secret-access"
    );
    f.auth.refresh_due_credentials().await.unwrap();
    assert_eq!(f.requests.lock().unwrap().len(), 1);
    assert!(
        f.auth
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .next_refresh_at
            .is_none()
    );
}

#[tokio::test]
async fn in_doubt_state_stops_after_twenty_attempts_even_with_explicit_transient_replies() {
    let mut f = Fixture::new(true).await;
    f.auth = f
        .auth
        .clone()
        .with_refresh_timeout(Duration::from_millis(20));
    f.advance_to_refresh();
    f.reply(StatusCode::OK, json!({}), Duration::from_secs(1));
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    for attempt in 2..=20 {
        let retry = f
            .auth
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .next_retry_at
            .unwrap();
        f.clock.store(retry, Ordering::SeqCst);
        f.reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"temporary"}),
            Duration::ZERO,
        );
        let result = f.auth.token("sub", Flow::Claude, None).await;
        if attempt == 20 {
            assert_eq!(
                result.unwrap_err().to_string(),
                "Refresh token state is unknown; sign in again"
            );
        } else {
            assert!(
                f.auth
                    .status("sub", Flow::Claude)
                    .await
                    .unwrap()
                    .refresh_in_doubt
            );
        }
    }
    assert!(!f.auth.status("sub", Flow::Claude).await.unwrap().connected);
    assert!(f.auth.token("sub", Flow::Claude, None).await.is_err());
    assert_eq!(f.requests.lock().unwrap().len(), 21);
}

#[tokio::test]
async fn restart_with_in_doubt_state_older_than_twenty_four_hours_disconnects_without_send() {
    let f = Fixture::new(true).await;
    f.advance_to_refresh();
    f.reply(StatusCode::OK, json!({}), Duration::from_secs(30));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            f.auth.token("sub", Flow::Claude, None)
        )
        .await
        .is_err()
    );
    f.clock
        .store(4_000_000 + 24 * 60 * 60 * 1000, Ordering::SeqCst);
    let c = f.clock.clone();
    let reloaded = OAuthManager::new(f.db.clone(), f.key.clone(), OAuthEndpoints::default())
        .unwrap()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_flow_config(Flow::Claude, f.config.clone());
    assert_eq!(
        reloaded
            .token("sub", Flow::Claude, None)
            .await
            .unwrap_err()
            .to_string(),
        "Refresh token state is unknown; sign in again"
    );
    assert!(
        !reloaded
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .connected
    );
    assert_eq!(f.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn refresh_metrics_and_logs_contain_fixed_results_but_no_tokens_or_secrets() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let metrics = recorder.handle();
    let _recorder = metrics::set_default_local_recorder(&recorder);
    let logs = LogBuffer(Arc::default());
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    // This integration-test binary installs one subscriber. A thread-local
    // subscriber can race callsite-interest caching in parallel tests.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let mut f = Fixture::new(true).await;
    f.config.client_secret = Some("secret-client".into());
    f.auth = f
        .auth
        .clone()
        .with_flow_config(Flow::Claude, f.config.clone());
    f.advance_to_refresh();
    f.reply(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"error":"secret-response"}),
        Duration::ZERO,
    );
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    f.clock.store(
        f.auth
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .next_retry_at
            .unwrap(),
        Ordering::SeqCst,
    );
    f.success();
    f.auth.token("sub", Flow::Claude, None).await.unwrap();
    f.reply(
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid_grant","error_description":"secret-error"}),
        Duration::ZERO,
    );
    assert!(
        f.auth
            .token("sub", Flow::Claude, Some("new-secret"))
            .await
            .is_err()
    );
    let status = serde_json::to_string(&f.auth.status("sub", Flow::Claude).await.unwrap()).unwrap();
    let metrics = metrics.render();
    assert!(metrics.contains("tokomotive_oauth_refresh_total"));
    assert!(metrics.contains("tokomotive_oauth_access_expiry_seconds"));
    for result in ["transient", "success", "disconnected"] {
        assert!(metrics.contains(&format!("result=\"{result}\"")));
    }
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("OAuth refresh finished"));
    for text in [&status, &metrics, &logs] {
        for secret in [
            "secret-access",
            "secret-refresh",
            "new-secret",
            "rotated-secret",
            "secret-client",
            "secret-response",
            "secret-error",
        ] {
            assert!(!text.contains(secret));
        }
    }
}
