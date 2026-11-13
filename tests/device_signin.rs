use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
};
use tokomotive::{
    db,
    oauth::{Flow, OAuthEndpoints, OAuthManager},
    settings::SecretKey,
};

#[tokio::test]
async fn device_pending_slow_down_and_success_store_only_encrypted_tokens() {
    let response = Arc::new(Mutex::new((StatusCode::FORBIDDEN, json!({}))));
    let polls = Arc::new(Mutex::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let (r, p) = (response.clone(), polls.clone());
    let id = format!("h.{}.s",URL_SAFE_NO_PAD.encode(json!({"email":"owner@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"acct","chatgpt_plan_type":"plus"}}).to_string()));
    let app = Router::new()
        .route("/api/accounts/deviceauth/usercode",post(|Json(v):Json<Value>| async move {
            assert_eq!(v,json!({"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}));
            Json(json!({"device_auth_id":"device-secret","user_code":"ABCD-12345","interval":"2"}))
        }))
        .route("/api/accounts/deviceauth/token",post(move |Json(v):Json<Value>| {let (r,p)=(r.clone(),p.clone());async move {
            assert_eq!(v,json!({"device_auth_id":"device-secret","user_code":"ABCD-12345"}));
            *p.lock().unwrap() += 1;
            let (status,body)=r.lock().unwrap().clone();(status,Json(body)).into_response()
        }}))
        .route("/oauth/token",post(move |axum::extract::Form(v):axum::extract::Form<std::collections::HashMap<String,String>>| {let id=id.clone();async move {
            assert_eq!(v["code_verifier"],"verifier-secret");
            assert!(v["redirect_uri"].ends_with("/deviceauth/callback"));
            Json(json!({"access_token":"device-access-secret","refresh_token":"device-refresh-secret","id_token":id,"expires_in":3600}))
        }}));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let database = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,auth,base_url) VALUES('legacy','\"openai\"','codex','https://chatgpt.com/backend-api/codex')").execute(&database).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let manager = OAuthManager::new(
        database.clone(),
        SecretKey::from_bytes([4; 32]),
        OAuthEndpoints {
            issuer,
            ..Default::default()
        },
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    let start = manager.start_device("legacy", Flow::Codex).await.unwrap();
    assert_eq!(start.user_code, "ABCD-12345");
    assert!(
        !serde_json::to_string(&start)
            .unwrap()
            .contains("device-secret")
    );
    assert!(
        manager
            .poll_device("legacy", Flow::Codex, &start.attempt)
            .await
            .unwrap()
    );
    assert_eq!(*polls.lock().unwrap(), 0);
    clock.store(3000, Ordering::SeqCst);
    assert!(
        manager
            .poll_device("legacy", Flow::Codex, &start.attempt)
            .await
            .unwrap()
    );
    assert_eq!(*polls.lock().unwrap(), 1);
    *response.lock().unwrap() = (StatusCode::TOO_MANY_REQUESTS, json!({"error":"slow_down"}));
    clock.store(5000, Ordering::SeqCst);
    manager
        .poll_device("legacy", Flow::Codex, &start.attempt)
        .await
        .unwrap();
    assert_eq!(
        manager
            .device_status("legacy", Flow::Codex, &start.attempt)
            .await
            .unwrap()
            .interval,
        7
    );
    clock.store(7000, Ordering::SeqCst);
    manager
        .poll_device("legacy", Flow::Codex, &start.attempt)
        .await
        .unwrap();
    assert_eq!(*polls.lock().unwrap(), 2);
    *response.lock().unwrap() = (
        StatusCode::OK,
        json!({"authorization_code":"auth-secret","code_verifier":"verifier-secret","code_challenge":URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(b"verifier-secret"))}),
    );
    clock.store(12000, Ordering::SeqCst);
    assert!(
        !manager
            .poll_device("legacy", Flow::Codex, &start.attempt)
            .await
            .unwrap()
    );
    assert_eq!(
        manager
            .device_status("legacy", Flow::Codex, &start.attempt)
            .await
            .unwrap()
            .state,
        "connected"
    );
    assert_eq!(
        manager.token("legacy", Flow::Codex, None).await.unwrap(),
        "device-access-secret"
    );
    let encrypted: String = sqlx::query_scalar("SELECT secret FROM oauth_credentials")
        .fetch_one(&database)
        .await
        .unwrap();
    assert!(
        !encrypted.contains("device-access-secret") && !encrypted.contains("device-refresh-secret")
    );
    manager.disconnect("legacy", Flow::Codex).await.unwrap();
    assert!(
        manager
            .device_status("legacy", Flow::Codex, &start.attempt)
            .await
            .is_err()
    );
    let old = manager.start_device("legacy", Flow::Codex).await.unwrap();
    let denied = manager.start_device("legacy", Flow::Codex).await.unwrap();
    assert!(
        manager
            .device_status("legacy", Flow::Codex, &old.attempt)
            .await
            .is_err()
    );
    *response.lock().unwrap() = (StatusCode::FORBIDDEN, json!({"error":"access_denied"}));
    clock.store(14000, Ordering::SeqCst);
    assert!(
        !manager
            .poll_device("legacy", Flow::Codex, &denied.attempt)
            .await
            .unwrap()
    );
    assert_eq!(
        manager
            .device_status("legacy", Flow::Codex, &denied.attempt)
            .await
            .unwrap()
            .state,
        "denied"
    );
    assert!(manager.token("legacy", Flow::Codex, None).await.is_err());
    let expiry = manager.start_device("legacy", Flow::Codex).await.unwrap();
    clock.store(expiry.expires_at, Ordering::SeqCst);
    let expired = manager
        .device_status("legacy", Flow::Codex, &expiry.attempt)
        .await
        .unwrap();
    assert_eq!(expired.state, "expired");
    assert_eq!(expired.remaining_secs, 0);
    assert!(
        !manager
            .poll_device("legacy", Flow::Codex, &expiry.attempt)
            .await
            .unwrap()
    );
    clock.store(expiry.expires_at + 60001, Ordering::SeqCst);
    assert!(
        manager
            .device_status("legacy", Flow::Codex, &expiry.attempt)
            .await
            .is_err()
    );
    let stale = manager.start_device("legacy", Flow::Codex).await.unwrap();
    let before = *polls.lock().unwrap();
    clock.fetch_add(16000, Ordering::SeqCst);
    assert!(
        !manager
            .poll_device("legacy", Flow::Codex, &stale.attempt)
            .await
            .unwrap()
    );
    assert!(
        manager
            .device_status("legacy", Flow::Codex, &stale.attempt)
            .await
            .is_err()
    );
    assert_eq!(*polls.lock().unwrap(), before);
    server.abort();
}
use sha2::Digest;

#[tokio::test]
async fn device_poll_never_shortens_the_issuers_interval() {
    let polls = Arc::new(Mutex::new(0));
    let p = polls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(|| async {
                Json(json!({"device_auth_id":"secret","user_code":"CODE","interval":"120"}))
            }),
        )
        .route(
            "/api/accounts/deviceauth/token",
            post(move || {
                let p = p.clone();
                async move {
                    *p.lock().unwrap() += 1;
                    (StatusCode::FORBIDDEN, Json(json!({})))
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let database = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,auth,base_url) VALUES('sharing','\"openai\"','chatgpt','https://api.openai.com'),('claude','\"anthropic\"','claude','https://api.anthropic.com')").execute(&database).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let manager = OAuthManager::new(
        database,
        SecretKey::from_bytes([4; 32]),
        OAuthEndpoints {
            issuer,
            ..Default::default()
        },
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    assert!(
        manager
            .start_device("sharing", Flow::Chatgpt)
            .await
            .is_err()
    );
    assert!(manager.start_device("claude", Flow::Claude).await.is_err());
    let a = manager.start_device("sharing", Flow::Codex).await.unwrap();
    clock.store(61000, Ordering::SeqCst);
    manager
        .device_status("sharing", Flow::Codex, &a.attempt)
        .await
        .unwrap();
    manager
        .poll_device("sharing", Flow::Codex, &a.attempt)
        .await
        .unwrap();
    assert_eq!(*polls.lock().unwrap(), 0);
    clock.store(121000, Ordering::SeqCst);
    manager
        .device_status("sharing", Flow::Codex, &a.attempt)
        .await
        .unwrap();
    manager
        .poll_device("sharing", Flow::Codex, &a.attempt)
        .await
        .unwrap();
    assert_eq!(*polls.lock().unwrap(), 1);
    // Changing to the paste flow cancels the private device attempt.
    manager.start("sharing", Flow::Codex).await.unwrap();
    assert!(
        manager
            .device_status("sharing", Flow::Codex, &a.attempt)
            .await
            .is_err()
    );
    server.abort();
}
