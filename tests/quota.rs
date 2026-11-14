use axum::http::HeaderMap;
use tokomotive::quota::parse_headers;

fn headers(values: &[(&str, &str)]) -> HeaderMap {
    values
        .iter()
        .map(|(k, v)| {
            (
                k.parse::<axum::http::HeaderName>().unwrap(),
                v.parse().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn account_health_keeps_safe_last_failure_and_upserts_windows() {
    use tokomotive::{db, quota};
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url) VALUES('test','\"openai\"','https://example.com')").execute(&db).await.unwrap();
    let account = quota::key_id("a-private-provider-key");
    let h = headers(&[
        ("x-ratelimit-limit-requests", "100"),
        ("x-ratelimit-remaining-requests", "20"),
        ("retry-after", "60"),
    ]);
    quota::record(&db, "test", &account, 429, &h, 1_700_000_000)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, Some("test"), 50).await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].state, "rate_limited");
    assert_eq!(accounts[0].until, Some(1_700_000_060));
    assert_eq!(accounts[0].windows.len(), 1);
    quota::record(&db, "test", &account, 401, &HeaderMap::new(), 1_700_000_001)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, Some("test"), 50).await.unwrap();
    assert_eq!(accounts[0].state, "credentials_invalid");
    assert_eq!(accounts[0].failure_at, Some(1_700_000_001));
    quota::record(&db, "test", &account, 200, &h, 1_700_000_002)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, Some("test"), 50).await.unwrap();
    assert_eq!(
        accounts[0].state, "credentials_invalid",
        "late inference success cannot replace credential recovery"
    );
    quota::recover_credentials(&db, "test", &account)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, Some("test"), 50).await.unwrap();
    assert_eq!(accounts[0].state, "ok");
    assert_eq!(
        accounts[0].last_failure.as_deref(),
        Some("Upstream rejected credentials (401)")
    );
    assert_eq!(accounts[0].windows.len(), 1);
    assert_eq!(accounts[0].windows[0].observed_at, 1_700_000_002);
    assert!(!format!("{accounts:?}").contains("a-private-provider-key"));
    sqlx::query("UPDATE providers SET name='renamed' WHERE name='test'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        quota::accounts(&db, Some("renamed"), 50)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DELETE FROM providers WHERE name='renamed'")
        .execute(&db)
        .await
        .unwrap();
    assert!(quota::accounts(&db, None, 50).await.unwrap().is_empty());
}

#[test]
fn anthropic_api_and_unified_subscription_windows() {
    let windows = parse_headers(
        &headers(&[
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "600"),
            ("anthropic-ratelimit-requests-reset", "2023-11-14T22:15:00Z"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1700018000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.8"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "0.3"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.1"),
        ]),
        1_700_000_000,
    );
    assert_eq!(windows.len(), 5);
    assert_eq!(windows[0].used_percent, Some(40.0));
    assert_eq!(windows[0].reset_at, Some(1_700_000_100));
    assert_eq!(windows[1].window, "5h");
    assert_eq!(windows[1].used_percent, Some(42.0));
    assert_eq!(windows[1].reset_at, Some(1_700_018_000));
    assert_eq!(windows[3].window, "7d_oi");
}

#[test]
fn codex_percent_and_reset_variants() {
    let windows = parse_headers(
        &headers(&[
            ("x-codex-primary-used-percent", "63"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1700018000"),
            ("x-codex-secondary-used-percent", "12"),
            ("x-codex-secondary-reset-after-seconds", "60"),
        ]),
        1_700_000_000,
    );
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].window, "codex-primary");
    assert_eq!(windows[0].used_percent, Some(63.0));
    assert_eq!(windows[0].reset_at, Some(1_700_018_000));
    assert_eq!(windows[1].reset_at, Some(1_700_000_060));
}

#[test]
fn malformed_values_do_not_create_unbounded_or_nonfinite_windows() {
    let windows = parse_headers(
        &headers(&[
            ("x-codex-primary-used-percent", "NaN"),
            ("anthropic-ratelimit-unified-5h-utilization", "-0.1"),
            ("x-ratelimit-limit-requests", "0"),
            ("x-ratelimit-remaining-requests", "20"),
            ("x-ratelimit-reset-requests", "99999999999999999999h"),
            ("anthropic-ratelimit-unified-evil-utilization", "0.5"),
        ]),
        1_700_000_000,
    );
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].used_percent, None);
    assert_eq!(windows[0].reset_at, None);
}

#[test]
fn empty_reset_headers_are_not_observations() {
    assert!(
        parse_headers(
            &headers(&[("x-ratelimit-reset-requests", "")]),
            1_700_000_000
        )
        .is_empty()
    );
}

#[tokio::test]
async fn disconnected_subscription_shows_disabled_without_inventing_usage() {
    let cfg = toml::from_str(
        r#"
[server]
database="sqlite::memory:"
admin_password="pw"
admin_password_env="TOKOMOTIVE_QUOTA_TEST_UNSET"
[[providers]]
name="disconnected"
kind="anthropic"
auth="claude"
base_url="https://api.anthropic.com"
"#,
    )
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::db::create_session(&st.db, "quota-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, tokomotive::router(st)).await.unwrap() });
    let html = reqwest::Client::new()
        .get(format!("{base}/admin/providers"))
        .header("cookie", "tokomotive_session=quota-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Disabled"));
    assert!(html.contains("No active subscription sign-in"));
    assert!(html.contains("No quota snapshot yet"));
    server.abort();
}

#[tokio::test]
async fn cached_plan_does_not_replace_a_newer_header_snapshot() {
    use tokomotive::{db, oauth, quota};
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url) VALUES('test','\"openai\"','https://example.com')").execute(&db).await.unwrap();
    quota::record(
        &db,
        "test",
        "signin",
        200,
        &headers(&[("x-codex-primary-used-percent", "80")]),
        1_700_000_010,
    )
    .await
    .unwrap();
    let usage = oauth::parse_codex_usage(
        &serde_json::json!({"rate_limit":{"primary_window":{"used_percent":42}}}),
        1_700_000_000_000,
    )
    .unwrap();
    quota::save_plan(&db, "test", "signin", &usage, usage.observed_at / 1000)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, None, 10).await.unwrap();
    assert_eq!(accounts[0].windows[0].quota.used_percent, Some(80.0));
    assert_eq!(accounts[0].windows[0].observed_at, 1_700_000_010);
}

#[test]
fn copilot_quota_snapshots_ignore_unlimited_and_untrusted_names() {
    let usage = tokomotive::oauth::parse_copilot_usage(&serde_json::json!({
        "quota_reset_date":"2026-12-01",
        "quota_snapshots":{
            "premium_interactions":{"entitlement":300,"remaining":240,"percent_remaining":80,"unlimited":false},
            "chat":{"unlimited":true,"percent_remaining":100},
            "arbitrary-secret-name":{"entitlement":1,"remaining":0}
        }
    }), 1_700_000_000_000).unwrap();
    assert_eq!(usage.windows.len(), 1);
    assert_eq!(usage.windows[0].used_percent, 20.0);
    assert_eq!(usage.windows[0].remaining, Some(240.0));
    assert_eq!(usage.windows[0].limit, Some(300.0));
    assert_eq!(usage.windows[0].resets_at, Some(1796083200000));
}

#[test]
fn retry_after_and_status_only_headers_are_safe_and_explicit() {
    use tokomotive::quota::retry_at;
    let now = 1_700_000_000;
    assert_eq!(
        retry_at(&headers(&[("retry-after", "120")]), now),
        Some(1_700_000_120)
    );
    assert_eq!(
        retry_at(
            &headers(&[("retry-after", "Tue, 14 Nov 2023 22:15:00 GMT")]),
            now
        ),
        Some(1_700_000_100)
    );
    assert_eq!(retry_at(&headers(&[("retry-after", "-1")]), now), None);
    let h = headers(&[
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-reset", "1700000200"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-reset", "1700600000"),
    ]);
    assert_eq!(parse_headers(&h, now)[0].used_percent, Some(100.0));
    assert_eq!(parse_headers(&h, now)[1].used_percent, None);
    assert_eq!(retry_at(&h, now), Some(1_700_000_200));
    assert_ne!(
        tokomotive::quota::key_id("first-same"),
        tokomotive::quota::key_id("second-same")
    );
}

#[tokio::test]
async fn copilot_existing_signin_reads_plan_and_stores_a_snapshot() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use tokomotive::{
        db,
        oauth::{Flow, OAuthEndpoints, OAuthManager},
        quota,
        settings::SecretKey,
    };
    let app=Router::new()
        .route("/device",post(||async{Json(json!({"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","interval":1}))}))
        .route("/token",post(||async{Json(json!({"access_token":"copilot-private","expires_in":3600}))}))
        .route("/user",get(|h:HeaderMap|async move {
            assert_eq!(h["authorization"],"Bearer copilot-private");
            Json(json!({"chat_enabled":true,"quota_reset_date":"2026-12-01","quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":150,"percent_remaining":50}}}))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth) VALUES('copilot','\"openai\"','https://api.githubcopilot.com','github_copilot')").execute(&db).await.unwrap();
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let c = clock.clone();
    let auth = OAuthManager::new(
        db.clone(),
        SecretKey::from_bytes([1; 32]),
        OAuthEndpoints {
            copilot_device: format!("{base}/device"),
            copilot_token: format!("{base}/token"),
            copilot_user: format!("{base}/user"),
            ..Default::default()
        },
    )
    .unwrap()
    .with_clock(move || c.load(Ordering::SeqCst));
    let attempt = auth
        .start_device("copilot", Flow::GithubCopilot)
        .await
        .unwrap();
    clock.fetch_add(1000, Ordering::SeqCst);
    auth.poll_device("copilot", Flow::GithubCopilot, &attempt.attempt)
        .await
        .unwrap();
    assert_eq!(
        auth.device_status("copilot", Flow::GithubCopilot, &attempt.attempt)
            .await
            .unwrap()
            .state,
        "connected"
    );
    let usage = auth.usage("copilot").await.unwrap();
    quota::record(
        &db,
        "copilot",
        "signin",
        401,
        &HeaderMap::new(),
        1_700_000_000,
    )
    .await
    .unwrap();
    quota::save_plan(&db, "copilot", "signin", &usage, usage.observed_at / 1000)
        .await
        .unwrap();
    let accounts = quota::accounts(&db, None, 10).await.unwrap();
    assert_eq!(
        accounts[0].state, "credentials_invalid",
        "usage requests do not clear inference failures"
    );
    assert_eq!(accounts[0].windows[0].quota.used_percent, Some(50.0));
    clock.fetch_add(10_000, Ordering::SeqCst);
    assert_eq!(
        auth.usage("copilot").await.unwrap().observed_at,
        usage.observed_at,
        "cached observations keep their original time"
    );
    let output = quota::metrics(&db).await.unwrap();
    assert!(output.contains("tokomotive_provider_quota_reset_seconds{"));
    assert!(output.contains("} 1796083200"));
    assert!(!output.contains("copilot-private"));
    mock.abort();
}

#[tokio::test]
async fn mock_proxy_updates_pages_health_and_metrics_without_secrets() {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    };
    let code = Arc::new(AtomicU16::new(200));
    let status = code.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let mock = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/chat/completions",post(move || {
            let status = status.clone();
            async move { (StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap(),
                [("anthropic-ratelimit-unified-5h-utilization","0.42"),("retry-after","60")],
                axum::Json(json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1},"error":{"message":"private-upstream-body"}}))).into_response() }
        }))).await.unwrap()
    });
    let cfg = toml::from_str(&format!(
        r#"
[server]
database="sqlite::memory:"
admin_password="pw"
admin_password_env="TOKOMOTIVE_QUOTA_TEST_UNSET"
[router]
max_attempts=1
[[providers]]
name="quota-test"
kind="openai"
base_url="http://{upstream}"
api_key="private-upstream-key"
[[users]]
name="user"
keys=["private-virtual-key"]
"#
    ))
    .unwrap();
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle();
    let st = tokomotive::init(&cfg, Some(handle)).await.unwrap();
    tokomotive::db::create_session(&st.db, "quota-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    for (status, expected) in [
        (200, "ok"),
        (401, "credentials_invalid"),
        (403, "credentials_invalid"),
        (429, "rate_limited"),
        (200, "ok"),
    ] {
        // Each status is a separate observation, not a request to a disabled
        // account. Explicitly recover before testing the next upstream status.
        tokomotive::quota::reenable(
            &st,
            "quota-test",
            &tokomotive::quota::key_id("private-upstream-key"),
        )
        .await
        .unwrap();
        code.store(status, Ordering::SeqCst);
        let response = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("private-virtual-key")
            .json(&json!({"model":"quota-test/model","messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        response.bytes().await.unwrap();
        let accounts = tokomotive::quota::accounts(&st.db, Some("quota-test"), 50)
            .await
            .unwrap();
        assert_eq!(accounts.len(), 1, "proxy must record upstream outcomes");
        assert_eq!(accounts[0].state, expected);
        if status == 429 {
            assert!(accounts[0].until.unwrap() >= chrono::Utc::now().timestamp() + 55);
        }
        for path in ["/admin/providers", "/admin", "/metrics"] {
            let output = http
                .get(format!("{base}{path}"))
                .header("cookie", "tokomotive_session=quota-session")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            for secret in [
                "private-upstream-key",
                "private-virtual-key",
                "private-upstream-body",
            ] {
                assert!(!output.contains(secret), "{path} leaked {secret}");
            }
            if path == "/metrics" {
                assert!(output.contains("tokomotive_provider_quota_used_ratio{"));
                assert!(output.contains("window=\"5h\"} 0.42"));
                assert!(output.contains(&format!("state=\"{expected}\"}} 1")));
            } else {
                assert!(output.contains("Subscription quota"), "{path} has no panel");
                assert!(output.contains("42%"), "{path} has no quota text");
                assert!(output.contains(expected), "{path} has no health state");
                if path == "/admin/providers" {
                    assert!(
                        output.contains(&format!(
                            "data-quota-key-id=\"{}\"",
                            tokomotive::quota::key_id("private-upstream-key")
                        )),
                        "key list must identify its quota account"
                    );
                }
            }
        }
    }
    mock.abort();
    server.abort();
}

#[test]
fn openai_windows_include_counts_and_duration_resets() {
    let headers: HeaderMap = [
        ("x-ratelimit-limit-requests", "100"),
        ("x-ratelimit-remaining-requests", "25"),
        ("x-ratelimit-reset-requests", "1m30s"),
    ]
    .into_iter()
    .map(|(k, v)| {
        (
            k.parse::<axum::http::HeaderName>().unwrap(),
            v.parse().unwrap(),
        )
    })
    .collect();
    let windows = parse_headers(&headers, 1_700_000_000);
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].window, "requests");
    assert_eq!(windows[0].used_percent, Some(75.0));
    assert_eq!(windows[0].remaining, Some(25.0));
    assert_eq!(windows[0].limit, Some(100.0));
    assert_eq!(windows[0].reset_at, Some(1_700_000_090));
}

/// The Overview shows only accounts that need attention or report quota. A
/// healthy API-key account without quota headers is noise there.
#[tokio::test]
async fn overview_hides_healthy_accounts_without_quota() {
    let cfg = toml::from_str(
        r#"
[server]
database="sqlite::memory:"
admin_password="pw"
admin_password_env="TOKOMOTIVE_QUOTA_TEST_UNSET"
[[providers]]
name="plain"
kind="openai"
base_url="http://127.0.0.1:9"
api_key="k1"
"#,
    )
    .unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::db::create_session(&st.db, "s", 1)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    let quiet = axum::http::HeaderMap::new();
    tokomotive::quota::record(&st.db, "plain", "healthy-account", 200, &quiet, now)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let page = |path: &'static str| {
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .get(format!("{base}{path}"))
                .header("cookie", "tokomotive_session=s")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    let html = page("/admin").await;
    assert!(
        !html.contains("healthy-account"),
        "healthy account shown on Overview"
    );
    assert!(!html.contains("No quota headers observed"));
    // An account that needs attention still appears.
    tokomotive::quota::record(&st.db, "plain", "limited-account", 429, &quiet, now)
        .await
        .unwrap();
    let html = page("/admin").await;
    assert!(html.contains("limited-account"));
    assert!(!html.contains("healthy-account"));
    // The Providers page keeps the full list.
    assert!(page("/admin/providers").await.contains("healthy-account"));
    server.abort();
}
