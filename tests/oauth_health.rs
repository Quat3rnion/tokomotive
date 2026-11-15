use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokomotive::{
    config::Config,
    db,
    oauth::{Flow, OAuthEndpoints, OAuthManager},
    provider::Endpoint,
    state::AppState,
};

struct Fixture {
    st: AppState,
    endpoints: OAuthEndpoints,
    url: String,
    transient: Arc<AtomicBool>,
    old_started: Arc<tokio::sync::Semaphore>,
    release_old: Arc<tokio::sync::Semaphore>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}
impl Fixture {
    async fn new(lifetime: i64, delay_first: bool) -> Self {
        let transient = Arc::new(AtomicBool::new(false));
        let token_calls = Arc::new(AtomicUsize::new(0));
        let old_calls = Arc::new(AtomicUsize::new(0));
        let old_started = Arc::new(tokio::sync::Semaphore::new(0));
        let release_old = Arc::new(tokio::sync::Semaphore::new(0));
        let (t, calls) = (transient.clone(), token_calls.clone());
        let (started, release) = (old_started.clone(), release_old.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/token", post(move || {
                let (t, calls) = (t.clone(), calls.clone());
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if t.load(Ordering::SeqCst) { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                    Json(json!({"access_token":if n == 0 {"old"} else {"new"},"refresh_token":format!("refresh-{n}"),"expires_in":if n == 0 {lifetime} else {3600}})).into_response()
                }
            }))
            .route("/v1/messages", post(move |headers: HeaderMap| {
                let (calls, started, release) = (old_calls.clone(), started.clone(), release.clone());
                async move {
                    if headers["authorization"] == "Bearer old" {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        started.add_permits(1);
                        if delay_first && n == 0 { release.acquire().await.unwrap().forget(); }
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(json!({"id":"msg","model":"m","role":"assistant","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).into_response()
                }
            }))
            .route("/v1/chat/completions", post(|| async { Json(json!({"model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}})) }));
        let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg: Config = toml::from_str(&format!(
            r#"
[server]
database='sqlite::memory:'
[router]
failure_threshold=1
cooldown_secs=600
[[providers]]
name='sub'
kind='anthropic'
auth='claude'
base_url='https://api.anthropic.com'
[[providers]]
name='paid'
kind='openai'
base_url='{base}'
[[routes]]
name='smart'
deployments=[{{provider='sub',model='m'}},{{provider='paid',model='m'}}]
[[users]]
name='user'
keys=['vk-test']
key_allowed_providers=['sub','paid']
"#
        ))
        .unwrap();
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::seed(&db, &cfg).await.unwrap();
        let endpoints = OAuthEndpoints {
            claude_token: format!("{base}/token"),
            claude_resource: base,
            ..Default::default()
        };
        let st = AppState::new_with_oauth_endpoints(&cfg, db, None, endpoints.clone())
            .await
            .unwrap();
        let a = st.oauth.start("sub", Flow::Claude).await.unwrap();
        let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&a.authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        st.oauth
            .finish("sub", Flow::Claude, &format!("code#{}", q["state"]))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = tokomotive::router(st.clone());
        let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            st,
            endpoints,
            url,
            transient,
            old_started,
            release_old,
            servers: vec![mock, proxy],
        }
    }
    fn request(&self) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth("vk-test")
            .json(&json!({"model":"smart","messages":[{"role":"user","content":"hello"}]}))
    }
    fn circuit_available(&self) -> bool {
        let table = self.st.router_table();
        let candidates = self.st.candidates(
            &table,
            &self.st.catalog(),
            Some("smart"),
            Endpoint::ChatCompletions,
            false,
        );
        self.st
            .deployment_available(&candidates[0], &table.settings)
    }
    async fn recover(&self) {
        let at = self
            .st
            .oauth
            .status("sub", Flow::Claude)
            .await
            .unwrap()
            .next_retry_at
            .unwrap();
        let auth = OAuthManager::new(
            self.st.db.clone(),
            self.st.secret_key.clone(),
            self.endpoints.clone(),
        )
        .unwrap()
        .with_clock(move || at);
        self.transient.store(false, Ordering::SeqCst);
        assert_eq!(
            auth.token("sub", Flow::Claude, Some("old")).await.unwrap(),
            "new"
        );
    }
    async fn provider(&self, req: reqwest::RequestBuilder) -> String {
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 200);
        let _ = r.bytes().await.unwrap();
        self.st.flush().await;
        sqlx::query_scalar("SELECT provider FROM ledger ORDER BY id DESC LIMIT 1")
            .fetch_one(&self.st.db)
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

#[tokio::test]
async fn refresh_retrying_before_send_does_not_open_circuit_with_healthy_fallback() {
    let f = Fixture::new(1, false).await;
    f.transient.store(true, Ordering::SeqCst);
    assert!(
        f.st.oauth
            .token("sub", Flow::Claude, Some("old"))
            .await
            .is_err()
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    for _ in 0..3 {
        assert_eq!(f.provider(f.request()).await, "paid");
    }
    assert!(
        f.circuit_available(),
        "local backoff must not open the deployment circuit"
    );
    f.recover().await;
    assert_eq!(f.provider(f.request()).await, "sub");
}

#[tokio::test]
async fn refresh_retrying_after_401_does_not_open_circuit_with_healthy_fallback() {
    let f = Fixture::new(3600, false).await;
    f.transient.store(true, Ordering::SeqCst);
    assert_eq!(f.provider(f.request()).await, "paid");
    assert!(
        f.circuit_available(),
        "401 refresh backoff must not open the deployment circuit"
    );
    f.recover().await;
    assert_eq!(f.provider(f.request()).await, "sub");
}

#[tokio::test]
async fn late_old_token_401_does_not_disable_recovered_account() {
    let f = Fixture::new(3600, true).await;
    let late = tokio::spawn(f.request().send());
    tokio::time::timeout(std::time::Duration::from_secs(5), f.old_started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    // This second old-token request refreshes and succeeds before the first 401 arrives.
    assert_eq!(f.provider(f.request()).await, "sub");
    f.release_old.add_permits(1);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), late)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.bytes().await.unwrap();
    let health: String = sqlx::query_scalar(
        "SELECT state FROM provider_health WHERE provider='sub' AND account='signin'",
    )
    .fetch_one(&f.st.db)
    .await
    .unwrap();
    assert_eq!(
        health, "ok",
        "a 401 for the old bearer must not overwrite recovered health"
    );
    assert_eq!(
        f.provider(f.request()).await,
        "sub",
        "later traffic must not skip the recovered account"
    );
}
