//! The gateway boundary: real HTTP, SQLite and a Streamable HTTP upstream.
use axum::{
    Json,
    body::Body,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokomotive::{config::Config, state::AppState};

#[derive(Clone, Default)]
struct Mock(
    Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    Arc<std::sync::atomic::AtomicBool>,
);
async fn upstream(State(mock): State<Mock>, headers: HeaderMap, Json(v): Json<Value>) -> Response {
    mock.0.lock().unwrap().push((headers, v.clone()));
    let result = match v["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}})
        }
        "notifications/initialized" => return axum::http::StatusCode::ACCEPTED.into_response(),
        "tools/list" => {
            let mut page = json!({"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}]});
            if mock.1.load(std::sync::atomic::Ordering::Relaxed) {
                if v["params"]["cursor"].is_string() {
                    page["tools"][0]["name"] = json!("other");
                } else {
                    for n in 1..100 {
                        page["tools"].as_array_mut().unwrap().push(
                            json!({"name":format!("tool{n:03}"),"inputSchema":{"type":"object"}}),
                        );
                    }
                    page["nextCursor"] = json!("opaque-next-page");
                }
            }
            page
        }
        "tools/call" => json!({"content":[{"type":"text","text":"done"}]}),
        _ => panic!("unexpected upstream request {v}"),
    };
    if v["method"] == "tools/call" {
        match v["params"]["arguments"]["mode"].as_str() {
            Some("redirect") => return (axum::http::StatusCode::FOUND, [("location","/trap")]).into_response(),
            Some("big") => return Json(json!({"jsonrpc":"2.0","id":v["id"],"result":{"content":[{"type":"text","text":"x".repeat(4*1024*1024)}]}})).into_response(),
            Some("sse" | "big_event") => {
                let result = if v["params"]["arguments"]["mode"] == "big_event" { json!({"content":[{"type":"text","text":"x".repeat(1024*1024)}]}) } else { result };
                let wire = format!("event: message\r\ndata: {}\r\n\r\n", json!({"jsonrpc":"2.0","id":v["id"],"result":result}));
                let chunks = wire.as_bytes().chunks(317).map(|b|Ok::<_,std::io::Error>(b.to_vec())).collect::<Vec<_>>();
                let stream = futures_util::stream::iter(chunks).chain(futures_util::stream::pending());
                return ([("content-type","text/event-stream")], Body::from_stream(stream)).into_response();
            }
            _ => {},
        }
    }
    (
        [("mcp-session-id", "upstream-1")],
        Json(json!({"jsonrpc":"2.0","id":v["id"],"result":result})),
    )
        .into_response()
}
struct Fixture {
    _dir: tempfile::TempDir,
    st: AppState,
    base: String,
    upstream: String,
    key: String,
    key_id: i64,
    mock: Mock,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn register(&self, name: &str) {
        sqlx::query("INSERT INTO mcp_servers(name,url,allow_private,enabled) VALUES(?,?,1,1)")
            .bind(name)
            .bind(&self.upstream)
            .execute(&self.st.db)
            .await
            .unwrap();
    }
    async fn rpc(&self, session: &str, method: &str, params: Value) -> Value {
        self.post(method, 2)
            .header("mcp-session-id", session)
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg: Config = toml::from_str(&format!(
            r#"[server]
database = "sqlite://{}"
admin_password = "pw"
admin_password_env = "MCP_TEST_UNSET"
"#,
            dir.path().join("db.sqlite").display()
        ))
        .unwrap();
        let st = tokomotive::init(&cfg, None).await.unwrap();
        let user = tokomotive::db::create_user(&st.db, "test", None, None, false)
            .await
            .unwrap();
        let (key_id, key) = tokomotive::db::create_key(&st.db, user, "test", None)
            .await
            .unwrap();
        let mock = Mock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/mcp", post(upstream))
            .route(
                "/trap",
                get(|State(m): State<Mock>| async move {
                    m.0.lock()
                        .unwrap()
                        .push((HeaderMap::new(), json!({"method":"TRAP"})));
                    "trap"
                }),
            )
            .with_state(mock.clone());
        let a = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = tokomotive::router(st.clone());
        let b = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _dir: dir,
            st,
            base,
            upstream: upstream_url,
            key,
            key_id,
            mock,
            tasks: vec![a, b],
        }
    }
    fn post(&self, method: &str, id: i64) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(format!("{}/mcp", self.base))
            .bearer_auth(&self.key)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":{}}))
    }
    async fn initialize(&self) -> String {
        let r = self.post("initialize", 1).json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let session = r.headers()["mcp-session-id"].to_str().unwrap().to_owned();
        assert_eq!(
            r.json::<Value>().await.unwrap()["result"]["protocolVersion"],
            "2025-06-18"
        );
        let r = self
            .post("notifications/initialized", 0)
            .header("mcp-session-id", &session)
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);
        session
    }
}

#[tokio::test]
async fn initialize_handshake_and_auth_required() {
    let f = Fixture::new().await;
    f.initialize().await;
    let r = reqwest::Client::new()
        .post(format!("{}/mcp", f.base))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn aggregate_prefix_route_default_deny_and_log() {
    let f = Fixture::new().await;
    f.register("alpha").await;
    f.register("beta").await;
    let session = f.initialize().await;
    assert_eq!(
        f.rpc(&session, "tools/list", json!({})).await["result"]["tools"],
        json!([])
    );
    sqlx::query("UPDATE virtual_keys SET mcp_allow='[\"alpha/echo\",\"beta/*\"]' WHERE id=?")
        .bind(f.key_id)
        .execute(&f.st.db)
        .await
        .unwrap();
    let listed = f.rpc(&session, "tools/list", json!({})).await;
    assert_eq!(listed["result"]["tools"][0]["name"], "alpha__echo");
    assert_eq!(listed["result"]["tools"][1]["name"], "beta__echo");
    let called = f
        .rpc(
            &session,
            "tools/call",
            json!({"name":"beta__echo","arguments":{"private":"do not store"}}),
        )
        .await;
    assert_eq!(called["result"]["content"][0]["text"], "done");
    assert!(
        f.mock
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(h, v)| v["method"] == "tools/call"
                && v["params"]["name"] == "echo"
                && h["mcp-session-id"] == "upstream-1")
    );
    let denied = f
        .rpc(&session, "tools/call", json!({"name":"alpha__forbidden"}))
        .await;
    assert_eq!(denied["error"]["code"], -32602);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_calls WHERE key_id=?")
        .bind(f.key_id)
        .fetch_one(&f.st.db)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let seen = f.mock.0.lock().unwrap();
    assert_eq!(seen[0].1["method"], "initialize");
    assert_eq!(seen[1].1["method"], "notifications/initialized");
    assert_eq!(seen[2].1["method"], "tools/list");
    assert_eq!(seen[2].0["mcp-protocol-version"], "2025-06-18");
}

#[tokio::test]
async fn sse_caps_redirects_team_intersection_and_rpm() {
    let f = Fixture::new().await;
    f.register("alpha").await;
    sqlx::query("UPDATE virtual_keys SET mcp_all=1 WHERE id=?")
        .bind(f.key_id)
        .execute(&f.st.db)
        .await
        .unwrap();
    let session = f.initialize().await;
    let sse = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.rpc(
            &session,
            "tools/call",
            json!({"name":"alpha__echo","arguments":{"mode":"sse"}}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(sse["result"]["content"][0]["text"], "done");
    for mode in ["redirect", "big", "big_event"] {
        let result = f
            .rpc(
                &session,
                "tools/call",
                json!({"name":"alpha__echo","arguments":{"mode":mode}}),
            )
            .await;
        assert_eq!(result["error"]["code"], -32603, "{mode}: {result}");
    }
    assert!(
        !f.mock
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(_, v)| v["method"] == "TRAP")
    );
    let team = tokomotive::db::create_team(&f.st.db, "restricted")
        .await
        .unwrap();
    sqlx::query("UPDATE users SET team_id=?")
        .bind(team)
        .execute(&f.st.db)
        .await
        .unwrap();
    assert_eq!(
        f.rpc(&session, "tools/list", json!({})).await["result"]["tools"],
        json!([])
    );
    assert_eq!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await["error"]["code"],
        -32602
    );
    sqlx::query("UPDATE teams SET mcp_allow='[\"alpha/echo\"]'")
        .execute(&f.st.db)
        .await
        .unwrap();
    assert_eq!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await["result"]["content"][0]["text"],
        "done"
    );
    sqlx::query("UPDATE virtual_keys SET rpm_limit=0 WHERE id=?")
        .bind(f.key_id)
        .execute(&f.st.db)
        .await
        .unwrap();
    assert_eq!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await["error"]["code"],
        -32000
    );
    let classes: Vec<String> = sqlx::query_scalar("SELECT error_class FROM mcp_calls ORDER BY id")
        .fetch_all(&f.st.db)
        .await
        .unwrap();
    assert_eq!(
        classes,
        vec![
            "",
            "upstream_http",
            "response_cap",
            "event_cap",
            "denied",
            "",
            "rate_limit"
        ]
    );
}

#[tokio::test]
async fn session_isolation_bounds_and_transport_headers() {
    let f = Fixture::new().await;
    let session = f.initialize().await;
    let ping = f.rpc(&session, "ping", json!({})).await;
    assert_eq!(ping["result"], json!({}));
    let r = f
        .post("ping", 2)
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = f
        .post("ping", 2)
        .header("mcp-session-id", &session)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = reqwest::Client::new()
        .post(format!("{}/mcp/alpha", f.base))
        .bearer_auth(&f.key)
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    for _ in 1..16 {
        f.initialize().await;
    }
    assert_eq!(f.post("initialize",1).json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{}}})).send().await.unwrap().status(),503);
    assert_eq!(
        reqwest::Client::new()
            .delete(format!("{}/mcp", f.base))
            .bearer_auth(&f.key)
            .header("mcp-session-id", &session)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        f.post("ping", 2)
            .header("mcp-session-id", &session)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn dashboard_encrypts_and_masks_credentials_and_picker_access() {
    let f = Fixture::new().await;
    let team = tokomotive::db::create_team(&f.st.db, "MCP team")
        .await
        .unwrap();
    tokomotive::db::create_session(&f.st.db, "mcp-admin", 1)
        .await
        .unwrap();
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c
        .post(format!("{}/admin/mcp", f.base))
        .header("cookie", "tokomotive_session=mcp-admin")
        .header("origin", &f.base)
        .form(&[
            ("name", "alpha"),
            ("url", f.upstream.as_str()),
            ("auth", "bearer"),
            ("secret", "private-upstream-secret"),
            ("allow_private", "on"),
            ("enabled", "on"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let secret: String = sqlx::query_scalar("SELECT secret FROM mcp_servers")
        .fetch_one(&f.st.db)
        .await
        .unwrap();
    assert!(!secret.contains("private-upstream-secret"));
    assert_eq!(
        f.st.secret_key.decrypt(&secret).unwrap(),
        "private-upstream-secret"
    );
    let r = c
        .post(format!("{}/admin/mcp/1/test", f.base))
        .header("cookie", "tokomotive_session=mcp-admin")
        .header("origin", &f.base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let html = c
        .get(format!("{}/admin/mcp", f.base))
        .header("cookie", "tokomotive_session=mcp-admin")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Echo text"));
    assert!(!html.contains("private-upstream-secret"));
    assert!(!html.contains(&secret));
    for path in ["/admin/keys", "/admin/teams"] {
        let html = c
            .get(format!("{}{path}", f.base))
            .header("cookie", "tokomotive_session=mcp-admin")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(html.contains("data-endpoint=\"/admin/mcp/search\""));
    }
    let choices: Value = c
        .get(format!("{}/admin/mcp/search", f.base))
        .header("cookie", "tokomotive_session=mcp-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(choices[0]["name"], "alpha/*");
    assert_eq!(choices[1]["name"], "alpha/echo");
    let r = c
        .post(format!("{}/admin/keys/{}/mcp", f.base, f.key_id))
        .header("cookie", "tokomotive_session=mcp-admin")
        .header("origin", &f.base)
        .form(&[("mcp_allow", "alpha/echo")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let r = c
        .post(format!("{}/admin/teams/{team}/mcp", f.base))
        .header("cookie", "tokomotive_session=mcp-admin")
        .header("origin", &f.base)
        .form(&[("mcp_allow", "alpha/echo")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    sqlx::query("UPDATE users SET team_id=?")
        .bind(team)
        .execute(&f.st.db)
        .await
        .unwrap();
    let session = f.initialize().await;
    assert_eq!(
        f.rpc(&session, "tools/list", json!({})).await["result"]["tools"][0]["name"],
        "alpha__echo"
    );
}

#[tokio::test]
async fn single_server_unprefixed_and_upstream_pagination() {
    let f = Fixture::new().await;
    f.register("alpha").await;
    f.mock.1.store(true, std::sync::atomic::Ordering::Relaxed);
    sqlx::query("UPDATE virtual_keys SET mcp_all=1")
        .execute(&f.st.db)
        .await
        .unwrap();
    let c = reqwest::Client::new();
    let endpoint = format!("{}/mcp/alpha", f.base);
    let response = c.post(&endpoint).bearer_auth(&f.key).json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{}}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let session = response.headers()["mcp-session-id"].to_str().unwrap();
    c.post(&endpoint)
        .bearer_auth(&f.key)
        .header("mcp-session-id", session)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    let response: Value = c
        .post(&endpoint)
        .bearer_auth(&f.key)
        .header("mcp-session-id", session)
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["result"]["tools"][0]["name"], "echo");
    assert_eq!(response["result"]["tools"][1]["name"], "other");
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 100);
    assert_eq!(response["result"]["nextCursor"], "100");
    let page = f
        .rpc(&f.initialize().await, "tools/list", json!({"cursor":"100"}))
        .await;
    assert_eq!(page["result"]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(page["result"]["tools"][0]["name"], "alpha__tool099");
    // A cursor is opaque and is never interpreted as a URL.
    let response = f
        .rpc(
            &f.initialize().await,
            "tools/list",
            json!({"cursor":"https://evil.example"}),
        )
        .await;
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn private_access_is_opt_in_and_custom_auth_stays_upstream() {
    let f = Fixture::new().await;
    f.register("alpha").await;
    sqlx::query("UPDATE virtual_keys SET mcp_all=1")
        .execute(&f.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE mcp_servers SET allow_private=0")
        .execute(&f.st.db)
        .await
        .unwrap();
    let session = f.initialize().await;
    assert_eq!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await["error"]["code"],
        -32603
    );
    assert!(f.mock.0.lock().unwrap().is_empty());
    let secret = f.st.secret_key.encrypt("upstream-custom-secret").unwrap();
    sqlx::query(
        "UPDATE mcp_servers SET allow_private=1,auth='header',header_name='X-Service-Key',secret=?",
    )
    .bind(secret)
    .execute(&f.st.db)
    .await
    .unwrap();
    assert!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await
            .get("result")
            .is_some()
    );
    assert!(
        f.mock
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|(h, _)| h["x-service-key"] == "upstream-custom-secret"
                && !h.contains_key("authorization"))
    );
    let other_user = tokomotive::db::create_user(&f.st.db, "other", None, None, false)
        .await
        .unwrap();
    let (_, other_key) = tokomotive::db::create_key(&f.st.db, other_user, "other", None)
        .await
        .unwrap();
    let r = reqwest::Client::new()
        .post(format!("{}/mcp", f.base))
        .header("x-api-key", other_key)
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    tokomotive::db::revoke_key(&f.st.db, f.key_id)
        .await
        .unwrap();
    assert_eq!(
        f.post("ping", 3)
            .header("mcp-session-id", &session)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn a_call_is_not_dispatched_without_a_durable_log_entry() {
    let f = Fixture::new().await;
    f.register("alpha").await;
    sqlx::query("UPDATE virtual_keys SET mcp_all=1")
        .execute(&f.st.db)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_mcp_log BEFORE INSERT ON mcp_calls BEGIN SELECT RAISE(FAIL,'log unavailable'); END").execute(&f.st.db).await.unwrap();
    let session = f.initialize().await;
    assert_eq!(
        f.rpc(&session, "tools/call", json!({"name":"alpha__echo"}))
            .await["error"]["code"],
        -32603
    );
    assert!(
        f.mock.0.lock().unwrap().is_empty(),
        "a side effect must not run without a call log"
    );
}

#[tokio::test]
async fn browser_origins_are_rejected_even_when_origin_matches_host() {
    let f = Fixture::new().await;
    let session = f.initialize().await;
    for origin in [f.base.as_str(), "null", "https://untrusted.example"] {
        let response = f
            .post("ping", 2)
            .header("mcp-session-id", &session)
            .header("origin", origin)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{origin}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["error"]["code"], -32000);
    }
}
