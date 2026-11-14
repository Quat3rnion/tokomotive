use chrono::{TimeZone, Utc};
use tokomotive::budgets::Period;

#[test]
fn utc_period_boundaries_include_month_end_and_monday() {
    let at = |y, m, d, h, n, s| Utc.with_ymd_and_hms(y, m, d, h, n, s).unwrap().timestamp();
    let now = at(2024, 2, 29, 23, 59, 59);
    assert_eq!(Period::None.bounds(now), (0, None));
    assert_eq!(
        Period::Daily.bounds(now),
        (at(2024, 2, 29, 0, 0, 0), Some(at(2024, 3, 1, 0, 0, 0)))
    );
    assert_eq!(
        Period::Monthly.bounds(now),
        (at(2024, 2, 1, 0, 0, 0), Some(at(2024, 3, 1, 0, 0, 0)))
    );
    assert_eq!(
        Period::Weekly.bounds(now),
        (at(2024, 2, 26, 0, 0, 0), Some(at(2024, 3, 4, 0, 0, 0)))
    );
    assert_eq!(
        Period::Weekly.bounds(at(2024, 3, 4, 0, 0, 0)).0,
        at(2024, 3, 4, 0, 0, 0)
    );
    assert_eq!(
        Period::Monthly.bounds(at(2024, 12, 31, 23, 59, 59)).1,
        Some(at(2025, 1, 1, 0, 0, 0))
    );
}

async fn app() -> (
    tokomotive::state::AppState,
    String,
    reqwest::Client,
    tokio::task::JoinHandle<()>,
) {
    let cfg: tokomotive::config::Config=toml::from_str("[server]\ndatabase='sqlite::memory:'\nadmin_password='pw'\nadmin_password_env='TOKOMOTIVE_TEST_UNSET_VAR'\n[[providers]]\nname='mock'\nkind='openai'\nbase_url='http://127.0.0.1:1'\napi_keys=['test']\n[[users]]\nname='test'\nkeys=['client']").unwrap();
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::db::create_session(&st.db, "test-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    (st, base, http, handle)
}

#[tokio::test]
async fn dashboard_sets_period_budgets_and_key_expiry() {
    let (st, base, http, handle) = app().await;
    let auth = tokomotive::db::lookup_key(&st.db, "client")
        .await
        .unwrap()
        .unwrap();
    let response = http
        .post(format!("{base}/admin/users/{}", auth.user_id))
        .header("cookie", "tokomotive_session=test-session")
        .form(&[("budget_usd", "5"), ("budget_period", "weekly")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let period: String = sqlx::query_scalar("SELECT budget_period FROM users WHERE id=?")
        .bind(auth.user_id)
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(period, "weekly");
    let response = http
        .post(format!("{base}/admin/keys/{}", auth.key_id))
        .header("cookie", "tokomotive_session=test-session")
        .form(&[
            ("budget_usd", "2"),
            ("budget_period", "daily"),
            ("expires_at", "2020-01-01T00:00"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    for (path, anthropic) in [("chat/completions", false), ("messages", true)] {
        let mut request = http
            .post(format!("{base}/v1/{path}"))
            .bearer_auth("client")
            .json(&serde_json::json!({"model":"mock/m","messages":[],"max_tokens":1}));
        if anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["message"], "Virtual Key expired");
        if anthropic {
            assert_eq!(body["type"], "error");
        } else {
            assert!(body.get("type").is_none());
        }
    }
    let html = http
        .get(format!("{base}/admin/keys"))
        .header("cookie", "tokomotive_session=test-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("expired") && html.contains("2020-01-01"));
    handle.abort();
}

#[tokio::test]
async fn webhook_settings_encrypt_urls_and_deliver_discord_and_slack() {
    let (st, base, http, handle) = app().await;
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/secret-token", listener.local_addr().unwrap());
    let mock = axum::Router::new().route(
        "/secret-token",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let send = send.clone();
            async move {
                send.send(v).unwrap();
                axum::http::StatusCode::OK
            }
        }),
    );
    let mock = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    for format in ["discord", "slack", "generic"] {
        let response = http
            .post(format!("{base}/admin/settings/alerts"))
            .header("cookie", "tokomotive_session=test-session")
            .form(&[
                ("url", url.as_str()),
                ("format", format),
                ("enabled", "on"),
                ("thresholds", "50,80,100"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
    }
    let stored: String = sqlx::query_scalar("SELECT url FROM alert_webhooks LIMIT 1")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert!(!stored.contains("secret-token"));
    let html = http
        .get(format!("{base}/admin/settings"))
        .header("cookie", "tokomotive_session=test-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("secret-token"));
    assert!(html.contains("127.0.0.1"));
    tokomotive::alerts::emit(
        &st,
        tokomotive::alerts::Event::new("test", "operator", "Test alert"),
    );
    st.flush().await;
    let mut payloads = vec![];
    while let Ok(v) = receive.try_recv() {
        payloads.push(v);
    }
    assert_eq!(payloads.len(), 3);
    assert!(payloads.iter().any(|v| v["content"] == "Test alert"));
    assert!(payloads.iter().any(|v| v["text"] == "Test alert"));
    assert!(
        payloads
            .iter()
            .any(|v| v["kind"] == "test" && v["message"] == "Test alert")
    );
    mock.abort();
    handle.abort();
}

async fn entry(
    st: &tokomotive::state::AppState,
    auth: &tokomotive::db::KeyAuth,
    ts: i64,
    price: i64,
) {
    tokomotive::db::insert_entry(
        &st.db,
        &tokomotive::db::NewEntry {
            cache_hit: false,
            ts_ms: ts * 1000,
            user_id: auth.user_id,
            key_id: auth.key_id,
            team_id: auth.team_id,
            provider: "mock".into(),
            subscription: false,
            endpoint: "chat/completions".into(),
            requested_model: None,
            model: None,
            status: 200,
            streamed: false,
            usage: Default::default(),
            reported_cost_micros: None,
            quote: tokomotive::catalog::Quote {
                price_micros: price,
                ..Default::default()
            },
            estimated: false,
            latency_ms: 1,
            ttft_ms: None,
            client_aborted: false,
            provider_key: None,
            attempts: 1,
            session_affinity: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reservations_use_current_ledger_period_and_enforce_every_subject() {
    use tokomotive::{
        budgets::{self, Subject},
        db,
    };
    let (st, base, http, handle) = app().await;
    let mut auth = db::lookup_key(&st.db, "client").await.unwrap().unwrap();
    let team = db::create_team(&st.db, "team").await.unwrap();
    sqlx::query("UPDATE users SET team_id=?,budget_micros=100,budget_period='daily' WHERE id=?")
        .bind(team)
        .bind(auth.user_id)
        .execute(&st.db)
        .await
        .unwrap();
    auth.team_id = Some(team);
    let now = Utc
        .with_ymd_and_hms(2024, 3, 1, 0, 0, 0)
        .unwrap()
        .timestamp();
    entry(&st, &auth, now - 1, 1000).await;
    entry(&st, &auth, now, 50).await;
    let hold = budgets::reserve(&st, &auth, 50, now)
        .await
        .unwrap()
        .unwrap();
    assert!(
        budgets::reserve(&st, &auth, 1, now)
            .await
            .unwrap()
            .is_none()
    );
    drop(hold);
    assert!(
        budgets::reserve(&st, &auth, 50, now)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        budgets::current(&st.db, Subject::User(auth.user_id), now)
            .await
            .unwrap()
            .spent,
        50
    );
    assert_eq!(
        db::spend_since(&st.db, auth.user_id, now).await.unwrap(),
        50
    );
    sqlx::query("UPDATE users SET budget_micros=NULL")
        .execute(&st.db)
        .await
        .unwrap();
    for (table, id) in [("teams", team), ("virtual_keys", auth.key_id)] {
        sqlx::query(&format!(
            "UPDATE {table} SET budget_micros=0,budget_period='daily' WHERE id=?"
        ))
        .bind(id)
        .execute(&st.db)
        .await
        .unwrap();
        let response = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth("client")
            .json(&serde_json::json!({"model":"mock/m","max_tokens":1}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 402, "{table}");
        sqlx::query(&format!("UPDATE {table} SET budget_micros=NULL WHERE id=?"))
            .bind(id)
            .execute(&st.db)
            .await
            .unwrap();
    }
    for (column, index, id) in [
        ("user_id", "ledger_user_ts", auth.user_id),
        ("key_id", "ledger_key_ts", auth.key_id),
        ("team_id", "ledger_team_ts", team),
    ] {
        let plan:Vec<(i64,i64,i64,String)>=sqlx::query_as(&format!("EXPLAIN QUERY PLAN SELECT SUM(price_micros) FROM ledger WHERE {column}=? AND ts_ms>=? AND ts_ms<?")).bind(id).bind(now*1000).bind((now+86400)*1000).fetch_all(&st.db).await.unwrap();
        assert!(
            plan.iter()
                .any(|row| row.3.contains(index) && row.3.contains("SEARCH")),
            "{plan:?}"
        );
    }
    handle.abort();
}

#[tokio::test]
async fn alert_thresholds_dedupe_persists_and_resets_each_period() {
    let (st, base, http, handle) = app().await;
    let auth = tokomotive::db::lookup_key(&st.db, "client")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE users SET budget_micros=100,budget_period='daily'")
        .execute(&st.db)
        .await
        .unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let mock = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let send = send.clone();
            async move {
                send.send(v).unwrap();
                axum::http::StatusCode::OK
            }
        }),
    );
    let mock = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    tokomotive::alerts::save(&st, None, &url, "generic", true)
        .await
        .unwrap();
    let now = Utc
        .with_ymd_and_hms(2024, 3, 1, 0, 0, 0)
        .unwrap()
        .timestamp();
    entry(&st, &auth, now, 80).await;
    let (a, b) = tokio::join!(
        tokomotive::alerts::check_budgets(&st, &auth, now),
        tokomotive::alerts::check_budgets(&st, &auth, now)
    );
    a.unwrap();
    b.unwrap();
    st.flush().await;
    let a = receive.try_recv().unwrap();
    let b = receive.try_recv().unwrap();
    let mut values = [
        a["details"]["threshold"].as_i64().unwrap(),
        b["details"]["threshold"].as_i64().unwrap(),
    ];
    values.sort();
    assert_eq!(values, [50, 80]);
    assert!(receive.try_recv().is_err());
    entry(&st, &auth, now, 20).await;
    tokomotive::alerts::check_budgets(&st, &auth, now)
        .await
        .unwrap();
    st.flush().await;
    assert_eq!(receive.try_recv().unwrap()["details"]["threshold"], 100);
    entry(&st, &auth, now + 86400, 50).await;
    tokomotive::alerts::check_budgets(&st, &auth, now + 86400)
        .await
        .unwrap();
    st.flush().await;
    assert_eq!(receive.try_recv().unwrap()["details"]["threshold"], 50);
    assert!(receive.try_recv().is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_dedupe")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert_eq!(count, 4);
    let r = http
        .post(format!("{base}/admin/settings/alerts/1/test"))
        .header("cookie", "tokomotive_session=test-session")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    st.flush().await;
    assert_eq!(receive.try_recv().unwrap()["kind"], "test");
    mock.abort();
    handle.abort();
}

#[tokio::test]
async fn invalid_budget_period_and_expiry_are_form_errors() {
    let (_st, base, http, handle) = app().await;
    for (path, field, value) in [
        ("/admin/users/1", "budget_period", "yearly"),
        ("/admin/keys/1", "expires_at", "not-a-date"),
    ] {
        let response = http
            .post(format!("{base}{path}"))
            .header("cookie", "tokomotive_session=test-session")
            .form(&[(field, value)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    handle.abort();
}

#[tokio::test]
async fn webhook_timeout_and_failure_are_bounded_and_do_not_block_api_requests() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let (st, base, http, handle) = app().await;
    let failures = Arc::new(AtomicUsize::new(0));
    let timeouts = Arc::new(AtomicUsize::new(0));
    let redirects = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let f = failures.clone();
    let t = timeouts.clone();
    let r = redirects.clone();
    let mock = axum::Router::new()
        .route(
            "/fail",
            axum::routing::post(move || {
                let f = f.clone();
                async move {
                    f.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .route(
            "/timeout",
            axum::routing::post(move || {
                let t = t.clone();
                async move {
                    t.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
                    axum::http::StatusCode::OK
                }
            }),
        )
        .route(
            "/redirect",
            axum::routing::post(|| async { axum::response::Redirect::temporary("/target") }),
        )
        .route(
            "/target",
            axum::routing::any(move || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::OK
                }
            }),
        );
    let mock = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    for path in ["fail", "timeout", "redirect"] {
        tokomotive::alerts::save(&st, None, &format!("{url}/{path}"), "generic", true)
            .await
            .unwrap();
    }
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        http.post(format!("{base}/admin/settings/alerts/2/test"))
            .header("cookie", "tokomotive_session=test-session")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 303);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        http.get(format!("{base}/v1/models"))
            .bearer_auth("client")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 200);
    tokomotive::alerts::emit_to(
        &st,
        tokomotive::alerts::Event::new("test", "operator", "Failure test"),
        Some(1),
    );
    tokomotive::alerts::emit_to(
        &st,
        tokomotive::alerts::Event::new("test", "operator", "Redirect test"),
        Some(3),
    );
    st.flush().await;
    assert_eq!(failures.load(Ordering::SeqCst), 3);
    assert_eq!(timeouts.load(Ordering::SeqCst), 3);
    assert_eq!(redirects.load(Ordering::SeqCst), 0);
    let hooks = tokomotive::alerts::list(&st.db).await.unwrap();
    assert_eq!(hooks[0].last_status, "HTTP 503");
    assert_eq!(hooks[1].last_status, "Timeout (10 seconds)");
    assert_eq!(hooks[2].last_status, "HTTP 307");
    mock.abort();
    handle.abort();
}

#[tokio::test]
async fn models_dev_price_alerts_emit_once() {
    let (st, _base, _http, handle) = app().await;
    let model = tokomotive::db::create_model(&st.db, "model", "mock", &[])
        .await
        .unwrap();
    sqlx::query("INSERT INTO models_dev_alerts(model_id,old_spec,new_spec,created_at) VALUES(?,'{}','{}',0)").bind(model).execute(&st.db).await.unwrap();
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let mock = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let send = send.clone();
            async move {
                send.send(v).unwrap();
                axum::http::StatusCode::OK
            }
        }),
    );
    let mock = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    tokomotive::alerts::save(&st, None, &url, "generic", true)
        .await
        .unwrap();
    tokomotive::alerts::models_dev(&st).await.unwrap();
    tokomotive::alerts::models_dev(&st).await.unwrap();
    st.flush().await;
    assert_eq!(receive.try_recv().unwrap()["kind"], "model_price_change");
    assert!(receive.try_recv().is_err());
    mock.abort();
    handle.abort();
}

#[tokio::test]
async fn simple_mode_hides_money_controls_but_keeps_and_enforces_key_expiry() {
    let (st, base, http, handle) = app().await;
    sqlx::query("UPDATE users SET budget_micros=100,budget_period='weekly'")
        .execute(&st.db)
        .await
        .unwrap();
    let response = http
        .post(format!("{base}/admin/settings/mode"))
        .header("cookie", "tokomotive_session=test-session")
        .form(&[("mode", "simple")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    for path in [
        "/admin/users",
        "/admin/teams",
        "/admin/keys",
        "/admin/settings",
    ] {
        let html = http
            .get(format!("{base}{path}"))
            .header("cookie", "tokomotive_session=test-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !html.contains("name=\"budget_usd\"")
                && !html.contains("name=\"budget_period\"")
                && !html.contains("name=\"thresholds\"")
        );
        if path == "/admin/keys" {
            assert!(html.contains("type=\"datetime-local\""));
        }
    }
    let response = http
        .post(format!("{base}/admin/keys/1"))
        .header("cookie", "tokomotive_session=test-session")
        .form(&[("expires_at", "2020-01-01T00:00")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let response = http
        .get(format!("{base}/v1/models"))
        .bearer_auth("client")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let budget: (Option<i64>, String) =
        sqlx::query_as("SELECT budget_micros,budget_period FROM users")
            .fetch_one(&st.db)
            .await
            .unwrap();
    assert_eq!(budget, (Some(100), "weekly".into()));
    handle.abort();
}
