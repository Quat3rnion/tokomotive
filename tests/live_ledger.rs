//! Live Ledger tests use the real dashboard session and SSE endpoint.
use futures_util::StreamExt;
use std::{future::IntoFuture, time::Duration};
use tokomotive::{config::Config, db, state::AppState};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    cookie: String,
}
impl Drop for App {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = toml::from_str(&format!(
            "[server]\ndatabase='sqlite://{}/db'\n[[users]]\nname='alice'\nkeys=['sk-alice']",
            dir.path().display()
        ))
        .unwrap();
        let st = tokomotive::init(&cfg, None).await.unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(l, tokomotive::router(st.clone())).into_future());
        let c = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let r = c
            .post(format!("{url}/setup"))
            .form(&[("username", "owner"), ("password", "owner-password")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 303);
        let cookie = r
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|h| h.to_str().unwrap())
            .find(|h| h.starts_with("tokomotive_session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            _dir: dir,
            st,
            url,
            c,
            server,
            cookie,
        }
    }
    async fn live(&self, query: &str) -> reqwest::Response {
        self.c
            .get(format!("{}/admin/ledger/live{query}", self.url))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }
}
fn entry() -> db::NewEntry {
    db::NewEntry {
        route_rule: None,
        cache_hit: false,
        semantic_cache_hit: false,
        semantic_cache_embed: false,
        ts_ms: db::now_ms(),
        user_id: 1,
        key_id: 1,
        team_id: None,
        provider: "mock".into(),
        subscription: false,
        endpoint: "chat_completions".into(),
        requested_model: Some("route".into()),
        model: Some("model".into()),
        status: 200,
        streamed: true,
        usage: tokomotive::usage::Usage {
            input: 12,
            output: 7,
            ..Default::default()
        },
        reported_cost_micros: None,
        quote: tokomotive::catalog::Quote {
            cost_micros: 123456,
            price_micros: 234567,
            ..Default::default()
        },
        estimated: false,
        latency_ms: 42,
        ttft_ms: Some(5),
        client_aborted: false,
        provider_key: None,
        attempts: 1,
        session_affinity: Some("pinned".into()),
    }
}
async fn frame(
    s: &mut (impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin),
) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut text = String::new();
        loop {
            text.push_str(std::str::from_utf8(&s.next().await.unwrap().unwrap()).unwrap());
            if text.contains("\n\n") {
                return text;
            }
        }
    })
    .await
    .expect("SSE frame")
}

#[tokio::test]
async fn live_requires_session_and_refuses_cross_origin() {
    let a = App::new().await;
    let r =
        a.c.get(format!("{}/admin/ledger/live", a.url))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/login");
    let r =
        a.c.get(format!("{}/admin/ledger/live", a.url))
            .header("cookie", &a.cookie)
            .header("origin", "https://other.example")
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn committed_entries_stream_shared_html_with_filters_and_no_secrets() {
    let a = App::new().await;
    let r = a
        .live("?user=alice&model=route&provider=mock&filter=errors&status=503&has_retries=false")
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let mut s = r.bytes_stream();
    assert_eq!(db::list_keys(&a.st.db).await.unwrap().len(), 1);
    db::insert_entry(&a.st.db, &entry()).await.unwrap();
    let mut e = entry();
    e.status = 503;
    let id = db::insert_entry(&a.st.db, &e).await.unwrap().unwrap();
    let f = frame(&mut s).await;
    assert!(f.contains("event: ledger"), "{f}");
    assert!(f.contains(&format!("id=\"e{id}\"")), "{f}");
    assert!(
        !f.contains("id=\"e1\""),
        "nonmatching entry must not stream"
    );
    assert!(f.contains("alice") && f.contains("503") && f.contains("42 ms") && f.contains("5 ms"));
    assert!(f.contains("$0.234567"));
    assert!(!f.contains("private-session-marker") && !f.contains("sk-alice"));
}

#[tokio::test]
async fn viewer_can_watch_simple_mode_without_money_or_captured_bodies() {
    let mut a = App::new().await;
    sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) SELECT 'reader',password_hash,'viewer' FROM admin_accounts LIMIT 1").execute(&a.st.db).await.unwrap();
    sqlx::query(
        "UPDATE settings SET value=json_set(value,'$.mode','simple') WHERE key='runtime_settings'",
    )
    .execute(&a.st.db)
    .await
    .unwrap();
    a.st.reload_settings().await.unwrap();
    // Use a current Viewer session (role changes normally invalidate sessions).
    let account = tokomotive::admin::by_username(&a.st.db, "reader")
        .await
        .unwrap()
        .unwrap();
    tokomotive::admin::create_session(&a.st.db, "viewer-token", 1, &account)
        .await
        .unwrap();
    a.cookie = "tokomotive_session=viewer-token".into();
    let r = a.live("").await;
    assert_eq!(r.status(), 200);
    let mut s = r.bytes_stream();
    let id = db::insert_entry(&a.st.db, &entry()).await.unwrap().unwrap();
    sqlx::query("INSERT INTO captured_bodies VALUES(?,?,?,?,0,0)")
        .bind(id)
        .bind(db::now_ms())
        .bind(a.st.secret_key.encrypt("private prompt marker").unwrap())
        .bind(a.st.secret_key.encrypt("private response marker").unwrap())
        .execute(&a.st.db)
        .await
        .unwrap();
    let f = frame(&mut s).await;
    for hidden in [
        "Price",
        "Cost",
        "$",
        "234567",
        "123456",
        "price_micros",
        "cost_micros",
        "private prompt marker",
        "private response marker",
        "sk-alice",
    ] {
        assert!(!f.contains(hidden), "{hidden}: {f}");
    }
    assert!(f.contains("alice") && f.contains("12 / 7"));
    let page =
        a.c.get(format!("{}/admin/ledger?entry={id}", a.url))
            .header("cookie", &a.cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(!page.contains("private prompt marker") && !page.contains("private response marker"));
}

#[tokio::test]
async fn last_event_id_resumes_and_old_or_foreign_cursor_gets_gap() {
    let a = App::new().await;
    let mut s = a.live("").await.bytes_stream();
    db::insert_entry(&a.st.db, &entry()).await.unwrap();
    let first = frame(&mut s).await;
    let cursor = first.lines().find_map(|s| s.strip_prefix("id: ")).unwrap();
    drop(s);
    for _ in 0..2 {
        db::insert_entry(&a.st.db, &entry()).await.unwrap();
    }
    let r =
        a.c.get(format!("{}/admin/ledger/live", a.url))
            .header("cookie", &a.cookie)
            .header("last-event-id", cursor)
            .send()
            .await
            .unwrap();
    let f = frame(&mut r.bytes_stream()).await;
    assert!(f.contains("id=\"e2\"") && !f.contains("id=\"e1\""), "{f}");
    for _ in 0..499 {
        db::insert_entry(&a.st.db, &entry()).await.unwrap();
    }
    for old in [cursor, "previous-process:1"] {
        let r =
            a.c.get(format!("{}/admin/ledger/live", a.url))
                .header("cookie", &a.cookie)
                .header("last-event-id", old)
                .send()
                .await
                .unwrap();
        assert!(frame(&mut r.bytes_stream()).await.contains("event: gap"));
    }
}

#[tokio::test]
async fn live_client_limit_is_four_per_session_and_releases_on_disconnect() {
    let a = App::new().await;
    let mut streams = Vec::new();
    for _ in 0..4 {
        let r = a.live("").await;
        assert_eq!(r.status(), 200);
        streams.push(r);
    }
    assert_eq!(a.live("").await.status(), 429);
    streams.pop();
    // The server notices cancellation asynchronously.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if a.live("").await.status() == 200 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn global_live_client_limit_is_thirty_two() {
    let a = App::new().await;
    let account = tokomotive::admin::by_username(&a.st.db, "owner")
        .await
        .unwrap()
        .unwrap();
    let mut streams = Vec::new();
    for session in 0..9 {
        let token = format!("limit-{session}");
        tokomotive::admin::create_session(&a.st.db, &token, 1, &account)
            .await
            .unwrap();
        for _ in 0..4 {
            let r =
                a.c.get(format!("{}/admin/ledger/live", a.url))
                    .header("cookie", format!("tokomotive_session={token}"))
                    .send()
                    .await
                    .unwrap();
            if session == 8 {
                assert_eq!(r.status(), 429);
            } else {
                assert_eq!(r.status(), 200);
                streams.push(r);
            }
        }
    }
}

#[tokio::test]
async fn spool_publishes_only_after_replay_commit_and_never_on_duplicate_replay() {
    let a = App::new().await;
    sqlx::query("UPDATE settings SET value=json_set(value,'$.writer_timeout_secs',1) WHERE key='runtime_settings'").execute(&a.st.db).await.unwrap();
    a.st.reload_settings().await.unwrap();
    let mut s = a.live("").await.bytes_stream();
    let writer = a.st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        db::insert_entry(&a.st.db, &entry())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), s.next())
            .await
            .is_err()
    );
    let path = a._dir.path().join("ledger-spool.jsonl");
    let record = std::fs::read(&path).unwrap();
    writer.rollback().await.unwrap();
    assert_eq!(db::replay_spool(&a.st.db).await.unwrap(), 1);
    assert!(frame(&mut s).await.contains("id=\"e1\""));
    std::fs::write(path.with_extension("replay.jsonl"), record).unwrap();
    assert_eq!(db::replay_spool(&a.st.db).await.unwrap(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), s.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn proxy_request_publishes_and_attempt_filters_match_without_diagnostics_race() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    let a = App::new().await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", l.local_addr().unwrap());
    let mock = tokio::spawn(axum::serve(l, Router::new().route("/v1/chat/completions", post(|| async { Json(json!({"model":"mock-model","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7}})) }))).into_future());
    let r =
        a.c.post(format!("{}/admin/providers", a.url))
            .header("cookie", &a.cookie)
            .form(&[
                ("name", "mock"),
                ("kind", "openai"),
                ("base_url", upstream.as_str()),
                ("api_key", "s3cr"),
            ])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303);
    let mut s = a
        .live("?provider=mock&model=mock/mock-model&provider_key=1&has_retries=false")
        .await
        .bytes_stream();
    let r =
        a.c.post(format!("{}/v1/chat/completions", a.url))
            .bearer_auth("sk-alice")
            .json(&json!({"model":"mock/mock-model","messages":[]}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let f = frame(&mut s).await;
    assert!(
        f.contains("event: ledger") && f.contains("mock-model") && f.contains("12 / 7"),
        "{f}"
    );
    assert!(!f.contains("s3cr") && !f.contains("sk-alice"));
    a.st.flush().await;
    mock.abort();
}

#[tokio::test]
async fn ledger_has_accessible_off_by_default_live_controls_and_bounded_client() {
    let a = App::new().await;
    let html =
        a.c.get(format!("{}/admin/ledger", a.url))
            .header("cookie", &a.cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    for marker in [
        "id=\"ledger-live\"",
        "aria-pressed=\"false\"",
        "id=\"ledger-pause\"",
        "id=\"ledger-live-announcement\"",
        "aria-live=\"polite\"",
        "EventSource",
        "localStorage",
        "10000",
        "200",
    ] {
        assert!(html.contains(marker), "{marker}");
    }
}

#[tokio::test]
async fn revoked_dashboard_session_cannot_receive_more_entries() {
    let a = App::new().await;
    let mut s = a.live("").await.bytes_stream();
    sqlx::query("DELETE FROM sessions")
        .execute(&a.st.db)
        .await
        .unwrap();
    db::insert_entry(&a.st.db, &entry()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), s.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn live_filters_cover_rule_dates_estimated_and_retries() {
    let a = App::new().await;
    let mut s = a.live("?rule=pilot&from=2026-11-15&to=2026-11-15&filter=estimated&has_retries=true&provider=mock&model=route&status=503").await.bytes_stream();
    let mut good = entry();
    good.ts_ms = 1794744000000; // 2026-11-15 12:00 UTC.
    good.estimated = true;
    good.status = 503;
    good.attempts = 3;
    good.route_rule = Some(tokomotive::route_rules::Matched {
        id: "pilot-id".into(),
        name: "pilot".into(),
        side: "canary".into(),
    });
    let mut excluded = Vec::new();
    let mut e = good.clone();
    e.route_rule.as_mut().unwrap().name = "other".into();
    excluded.push(e);
    let mut e = good.clone();
    e.ts_ms -= 86400000;
    excluded.push(e);
    let mut e = good.clone();
    e.ts_ms += 86400000;
    excluded.push(e);
    let mut e = good.clone();
    e.estimated = false;
    excluded.push(e);
    let mut e = good.clone();
    e.attempts = 1;
    excluded.push(e);
    let mut e = good.clone();
    e.provider = "other".into();
    excluded.push(e);
    let mut e = good.clone();
    e.requested_model = None;
    e.model = Some("other".into());
    excluded.push(e);
    let mut e = good.clone();
    e.status = 502;
    excluded.push(e);
    for e in excluded {
        db::insert_entry(&a.st.db, &e).await.unwrap();
    }
    let id = db::insert_entry(&a.st.db, &good).await.unwrap().unwrap();
    let f = frame(&mut s).await;
    assert!(
        f.contains(&format!("id=\"e{id}\"")) && f.contains("pilot / canary"),
        "{f}"
    );
}
