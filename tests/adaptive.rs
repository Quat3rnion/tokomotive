use axum::{Json, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokomotive::{config::Config, state::AppState};

struct App {
    st: AppState,
    url: String,
    c: reqwest::Client,
    clock: Arc<AtomicU64>,
    slow: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    tokens: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for App {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
async fn app(extra: &str, weight: u32) -> App {
    let clock = Arc::new(AtomicU64::new(0));
    let slow = Arc::new(AtomicU64::new(1000));
    let errors = Arc::new(AtomicU64::new(0));
    let tokens = Arc::new(AtomicU64::new(1));
    let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", up.local_addr().unwrap());
    let (time, delay) = (clock.clone(), slow.clone());
    let failures = errors.clone();
    let output = tokens.clone();
    let calls = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(async move {
        let router = axum::Router::new().route("/v1/chat/completions", post(move |Json(v): Json<Value>| {
            let (time, delay) = (time.clone(), delay.clone());
            let (failures,calls) = (failures.clone(),calls.clone());
            let output = output.clone();
            async move {
                let ms = if v["model"] == "fast" { 10 } else { delay.load(Ordering::SeqCst) };
                time.fetch_add(ms, Ordering::SeqCst);
                let every = failures.load(Ordering::SeqCst);
                if every == u64::MAX || (v["model"] == "slow" && every > 0 && calls.fetch_add(1,Ordering::SeqCst).is_multiple_of(every)) {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if v["stream"] == true {
                    let body = futures_util::stream::unfold(0,|i| async move {
                        if i > 1 { return None; }
                        if i > 0 { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }
                        let data = if i == 0 { "data: {\"model\":\"slow\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n" } else { "data: {\"model\":\"slow\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n" };
                        Some((Ok::<_,std::io::Error>(data),i+1))
                    });
                    return ([("content-type","text/event-stream")],axum::body::Body::from_stream(body)).into_response();
                }
                Json(json!({"model":v["model"],"choices":[{"message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":3,"completion_tokens":output.load(Ordering::SeqCst)}})).into_response()
            }
        }));
        axum::serve(up, router).await.unwrap();
    });
    let cfg: Config = toml::from_str(&format!(
        r#"
[server]
database='sqlite::memory:'
mode='simple'
admin_password='password123'
[router]
adaptive_half_life=2.0
{extra}
[[providers]]
name='up'
kind='openai'
base_url='{upstream}'
api_key='upstream-key'
[[providers]]
name='other'
kind='openai'
base_url='{upstream}'
api_key='other-key'
[[users]]
name='alice'
keys=['sk-alice']
[[routes]]
name='primary'
strategy='adaptive'
session_affinity=true
deployments=[{{provider='up',model='fast',weight=1}},{{provider='up',model='slow',weight={weight}}}]
[[routes]]
name='quota'
strategy='adaptive'
deployments=[{{provider='up',model='fast'}},{{provider='other',model='slow'}}]
"#
    ))
    .unwrap();
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
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
    let time = clock.clone();
    st.adaptive.set_sources(42, move || {
        std::time::Duration::from_millis(time.load(Ordering::SeqCst))
    });
    tokomotive::db::create_session(&st.db, "adaptive-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    App {
        st,
        url,
        c: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        clock,
        slow,
        errors,
        tokens,
        tasks: vec![task, proxy],
    }
}

#[tokio::test]
async fn adaptive_scores_fragment_is_live_and_viewer_readable() {
    let a = app("", 1).await;
    for _ in 0..60 {
        a.chat(None).await;
    }
    let r =
        a.c.post(format!("{}/admin/settings/accounts", a.url))
            .header("cookie", "tokomotive_session=adaptive-session")
            .header("origin", &a.url)
            .form(&[
                ("username", "reader"),
                ("role", "viewer"),
                ("password", "account-password"),
            ])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let r =
        a.c.post(format!("{}/admin/login", a.url))
            .header("origin", &a.url)
            .form(&[("username", "reader"), ("password", "account-password")])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303);
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    let r =
        a.c.get(format!("{}/admin/routes/primary/scores", a.url))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(
        html.contains("Success") && html.contains("Latency") && html.contains("Share"),
        "{html}"
    );
    assert!(html.contains("fast") && html.contains("slow"));
    assert!(!html.contains("<form") && !html.contains("upstream-key"));
    let page =
        a.c.get(format!("{}/admin/routes", a.url))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(page.contains("/admin/routes/primary/scores") && page.contains("every 5s"));
}

#[tokio::test]
async fn adaptive_settings_round_trip_and_validate() {
    let a = app("", 1).await;
    let page =
        a.c.get(format!("{}/admin/settings", a.url))
            .header("cookie", "tokomotive_session=adaptive-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    for field in [
        "adaptive_epsilon",
        "adaptive_half_life",
        "adaptive_prior_decay_minutes",
    ] {
        assert!(
            page.contains(&format!("name=\"{field}\"")),
            "Missing field: {field}"
        );
    }
    for (epsilon, status) in [("0.2", 303), ("1.1", 400), ("NaN", 400)] {
        let r =
            a.c.post(format!("{}/admin/settings", a.url))
                .header("cookie", "tokomotive_session=adaptive-session")
                .header("origin", &a.url)
                .form(&[
                    ("adaptive_epsilon", epsilon),
                    ("adaptive_half_life", "3"),
                    ("adaptive_prior_decay_minutes", "7"),
                    ("upstream_timeout_secs", "60"),
                    ("session_days", "30"),
                    ("failure_threshold", "3"),
                    ("cooldown_secs", "30"),
                    ("max_attempts", "8"),
                ])
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), status, "{}", r.text().await.unwrap());
    }
    let router = &a.st.settings().router;
    assert_eq!(router.adaptive_epsilon, 0.2);
    assert_eq!(router.adaptive_half_life, 3.0);
    assert_eq!(router.adaptive_prior_decay_minutes, 7.0);
}
impl App {
    async fn direct(&self, model: &str, stream: bool) -> reqwest::Response {
        self.c
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth("sk-alice")
            .json(
                &json!({"model":model,"messages":[{"role":"user","content":"hi"}],"stream":stream}),
            )
            .send()
            .await
            .unwrap()
    }
    fn scores(&self) -> Vec<tokomotive::adaptive::Score> {
        self.st.adaptive.scores(
            &self.st.router_table().routes[0],
            &self.st.settings().router,
            false,
        )
    }
    async fn chat(&self, session: Option<&str>) -> String {
        let mut req = self
            .c
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth("sk-alice")
            .json(&json!({"model":"primary","messages":[{"role":"user","content":"hi"}]}));
        req = req.header(
            "x-session-id",
            session
                .map(str::to_owned)
                .unwrap_or_else(|| format!("request-{}", self.clock.load(Ordering::SeqCst))),
        );
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        r.json::<Value>().await.unwrap()["model"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[tokio::test]
async fn adaptive_weight_multiplier_and_stale_prior_decay() {
    let a = app("adaptive_epsilon=0", 3).await;
    let scores = a.scores();
    assert!((scores[0].score - 0.00081).abs() < 1e-12);
    assert!((scores[1].score / scores[0].score - 3.0).abs() < 1e-12);
    a.slow.store(10, Ordering::SeqCst);
    for _ in 0..60 {
        for model in ["up/fast", "up/slow"] {
            assert_eq!(a.direct(model, false).await.status(), 200);
        }
    }
    let mut slow = 0;
    for _ in 0..200 {
        slow += usize::from(a.chat(None).await == "slow");
    }
    assert!((130..=175).contains(&slow), "weight 3 share: {slow}/200");
    a.direct("up/fast", false).await.bytes().await.unwrap();
    let scores = a.scores();
    assert!(scores[0].latency < 11.0);
    a.clock.fetch_add(300_000, Ordering::SeqCst);
    assert!(
        (a.scores()[0].latency - scores[0].latency).abs() < 1e-6,
        "Fresh estimate changed"
    );
    a.clock.fetch_add(300_000, Ordering::SeqCst);
    assert!(
        (a.scores()[0].latency - 505.0).abs() < 0.01,
        "One decay half-life"
    );
    a.clock.fetch_add(6_000_000, Ordering::SeqCst);
    let stale = a.scores();
    assert!((stale[0].score - 0.00081).abs() < 0.000001);
}

#[tokio::test]
async fn adaptive_penalizes_errors_and_never_picks_open_circuits() {
    let a = app("failure_threshold=1000\nadaptive_epsilon=0", 1).await;
    a.slow.store(10, Ordering::SeqCst);
    a.errors.store(2, Ordering::SeqCst);
    for _ in 0..80 {
        a.direct("up/slow", false).await.bytes().await.unwrap();
        a.direct("up/fast", false).await.bytes().await.unwrap();
    }
    let mut fast = 0;
    for _ in 0..200 {
        fast += usize::from(a.chat(None).await == "fast");
    }
    assert!(fast >= 145, "healthy share: {fast}/200");
    a.st.flush().await;
    let first_fast: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts WHERE ledger_id>160 AND attempt_index=1 AND deployment='fast'")
        .fetch_one(&a.st.db).await.unwrap();
    assert!(
        first_fast >= 145,
        "Healthy first-choice share: {first_fast}/200"
    );
    assert!(a.scores()[1].success < 0.8);

    let a = app(
        "failure_threshold=1\ncooldown_secs=600\nadaptive_epsilon=1",
        1,
    )
    .await;
    a.errors.store(1, Ordering::SeqCst);
    let failed = a.direct("up/slow", false).await;
    assert_eq!(failed.status(), 503);
    failed.bytes().await.unwrap();
    for _ in 0..40 {
        assert_eq!(a.chat(None).await, "fast");
    }
    a.errors.store(u64::MAX, Ordering::SeqCst);
    a.direct("up/fast", false).await.bytes().await.unwrap();
    let time = a.clock.load(Ordering::SeqCst);
    let response = a.direct("primary", false).await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        a.clock.load(Ordering::SeqCst),
        time,
        "All-open route contacted upstream"
    );
    a.st.flush().await;
    let attempts = tokomotive::observability::attempts(&a.st.db, 2)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].deployment, "fast");
}

#[tokio::test]
async fn adaptive_concurrency_and_client_abort_are_neutral() {
    let a = app("", 1).await;
    let gate =
        a.st.router_table()
            .providers
            .iter()
            .find(|p| p.name == "up")
            .unwrap()
            .gate
            .clone();
    gate.resize(1);
    let permit = gate.acquire("up", 0).await.unwrap();
    let before: Vec<_> = a.scores().iter().map(|s| s.score).collect();
    let response = a.direct("primary", false).await;
    assert_eq!(response.status(), 429);
    response.bytes().await.unwrap();
    assert_eq!(
        a.scores().iter().map(|s| s.score).collect::<Vec<_>>(),
        before
    );
    drop(permit);
    let mut response = a.direct("up/slow", true).await;
    response.chunk().await.unwrap().unwrap();
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(7), async {
        loop {
            a.st.flush().await;
            let aborted: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE client_aborted=1")
                    .fetch_one(&a.st.db)
                    .await
                    .unwrap();
            if aborted > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        a.scores().iter().map(|s| s.score).collect::<Vec<_>>(),
        before
    );
}

#[tokio::test]
async fn adaptive_normalizes_output_latency_and_keeps_ttft_separate() {
    let a = app("", 1).await;
    a.slow.store(1000, Ordering::SeqCst);
    a.tokens.store(100, Ordering::SeqCst);
    for _ in 0..40 {
        a.direct("up/slow", false).await.bytes().await.unwrap();
    }
    assert!(
        a.scores()[1].latency < 10.01,
        "Total latency was not normalized per output token"
    );
    let stream_prior = a.st.adaptive.scores(
        &a.st.router_table().routes[0],
        &a.st.settings().router,
        true,
    )[1]
    .latency;
    assert_eq!(stream_prior, 1000.0);
    a.slow.store(20, Ordering::SeqCst);
    a.direct("up/slow", true).await.bytes().await.unwrap();
    let stream = a.st.adaptive.scores(
        &a.st.router_table().routes[0],
        &a.st.settings().router,
        true,
    )[1]
    .latency;
    assert!(
        (stream - 712.96464556).abs() < 0.01,
        "Stream TTFT: {stream}"
    );
    assert!(
        a.scores()[1].latency < 10.01,
        "Stream overwrote non-stream latency"
    );
}

#[tokio::test]
async fn adaptive_rule_subset_and_trace_and_metric_are_visible() {
    let a = app("", 1).await;
    for _ in 0..60 {
        a.chat(None).await;
    }
    let r =
        a.c.post(format!("{}/admin/routes/primary/rules", a.url))
            .header("cookie", "tokomotive_session=adaptive-session")
            .header("origin", &a.url)
            .form(&[
                ("rule[0][name]", "only-slow"),
                ("rule[0][enabled]", "on"),
                ("rule[0][action]", "deployments"),
                ("rule[0][deployments]", r#"["up","slow"]"#),
            ])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303);
    for _ in 0..20 {
        assert_eq!(a.chat(None).await, "slow");
    }
    a.st.flush().await;
    let attempts = tokomotive::observability::attempts(&a.st.db, 80)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].deployment, "slow");
    assert_eq!(attempts[0].route_rule_name.as_deref(), Some("only-slow"));
    assert!(attempts[0].adaptive_score.is_some());
    let trace =
        a.c.get(format!("{}/admin/ledger?entry=80", a.url))
            .header("cookie", "tokomotive_session=adaptive-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(
        trace.contains("Adaptive score") && trace.contains("Exploration"),
        "Trace has no adaptive diagnostics"
    );
    let metrics =
        a.c.get(format!("{}/metrics", a.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("tokomotive_deployment_score{")
                && l.contains("route=\"primary\"")
                && l.contains("provider=\"up\"")
                && l.contains("model=\"slow\"")),
        "{metrics}"
    );
}

#[tokio::test]
async fn adaptive_explores_only_quota_eligible_deployments() {
    let a = app("adaptive_epsilon=1", 1).await;
    tokomotive::quota::record(
        &a.st.db,
        "up",
        &tokomotive::quota::key_id("upstream-key"),
        401,
        &axum::http::HeaderMap::new(),
        chrono::Utc::now().timestamp(),
    )
    .await
    .unwrap();
    for _ in 0..20 {
        let r = a.direct("quota", false).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["model"], "slow");
    }
    a.st.flush().await;
    let explores: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_attempts WHERE skip_reason='' AND adaptive_exploration=1",
    )
    .fetch_one(&a.st.db)
    .await
    .unwrap();
    assert_eq!(explores, 20, "Skipped accounts diluted uniform exploration");
}
#[tokio::test]
async fn adaptive_traffic_shifts_to_faster_deployment() {
    let a = app("", 1).await;
    let mut fast = 0;
    for i in 0..160 {
        if a.chat(None).await == "fast" && i >= 60 {
            fast += 1;
        }
    }
    assert!(fast >= 85, "fast requests in final 100: {fast}");
}

#[tokio::test]
async fn adaptive_recovers_and_explores_and_keeps_sessions() {
    let a = app("adaptive_epsilon=0.1", 1).await;
    for _ in 0..140 {
        a.chat(None).await;
    }
    let pinned = a.chat(Some("conversation")).await;
    a.slow.store(1, Ordering::SeqCst);
    let mut recovered = 0;
    for i in 0..500 {
        if a.chat(None).await == "slow" && i >= 400 {
            recovered += 1;
        }
    }
    assert!(recovered >= 75, "recovered requests: {recovered}");
    for _ in 0..20 {
        assert_eq!(a.chat(Some("conversation")).await, pinned);
    }
    a.st.flush().await;
    let picks: Vec<(Option<f64>,Option<bool>)> = sqlx::query_as("SELECT adaptive_score,adaptive_exploration FROM request_attempts WHERE skip_reason='' ORDER BY ledger_id LIMIT 640")
        .fetch_all(&a.st.db).await.unwrap();
    assert_eq!(picks.len(), 640);
    assert!(
        picks
            .iter()
            .all(|(score, _)| score.is_some_and(|s| s > 0.0))
    );
    let exploration = picks.iter().filter(|(_, e)| *e == Some(true)).count();
    assert!(
        (40..=90).contains(&exploration),
        "exploration: {exploration}/640"
    );
    let pinned_exploration: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_attempts WHERE ledger_id>641 AND adaptive_exploration=1",
    )
    .fetch_one(&a.st.db)
    .await
    .unwrap();
    assert_eq!(pinned_exploration, 0, "A pin is not an exploration pick");
}
