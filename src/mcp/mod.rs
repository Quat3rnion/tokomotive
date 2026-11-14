//! MCP Streamable HTTP gateway. The transport is deliberately POST-only.
//! Spec: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
//! Lifecycle: https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle
use crate::{db::KeyAuth, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub mod data;
pub mod upstream;
type Upstreams =
    Arc<tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<upstream::Connection>>>>>;

pub const VERSION: &str = "2025-06-18";
const BODY_CAP: usize = 1024 * 1024;
const SESSION_TTL: Duration = Duration::from_secs(1800);
const MAX_SESSIONS: usize = 1024;
const MAX_KEY_SESSIONS: usize = 16;

struct Session {
    key: i64,
    endpoint: String,
    touched: Instant,
    initialized: bool,
    upstreams: Upstreams,
}
pub struct Sessions {
    inner: Mutex<HashMap<String, Session>>,
    ttl: Duration,
    max: usize,
    per_key: usize,
}
impl Default for Sessions {
    fn default() -> Self {
        Self::with_limits(SESSION_TTL, MAX_SESSIONS, MAX_KEY_SESSIONS)
    }
}
impl Sessions {
    /// Smaller limits support embedded deployments and deterministic expiry checks.
    /// Limits cannot exceed the gateway's hard bounds.
    pub fn with_limits(ttl: Duration, max: usize, per_key: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: ttl.min(SESSION_TTL),
            max: max.min(MAX_SESSIONS),
            per_key: per_key.min(MAX_KEY_SESSIONS),
        }
    }
    pub fn create(&self, key: i64, endpoint: &str) -> Option<String> {
        let mut sessions = self.inner.lock().expect("MCP sessions");
        sessions.retain(|_, s| s.touched.elapsed() < self.ttl);
        if sessions.len() >= self.max
            || sessions.values().filter(|s| s.key == key).count() >= self.per_key
        {
            return None;
        }
        let id = hex::encode(rand::random::<[u8; 32]>());
        sessions.insert(
            id.clone(),
            Session {
                key,
                endpoint: endpoint.into(),
                touched: Instant::now(),
                initialized: false,
                upstreams: Default::default(),
            },
        );
        Some(id)
    }
    pub fn check(&self, id: &str, key: i64, endpoint: &str, initialized: bool) -> bool {
        let mut sessions = self.inner.lock().expect("MCP sessions");
        sessions.retain(|_, s| s.touched.elapsed() < self.ttl);
        let Some(s) = sessions
            .get_mut(id)
            .filter(|s| s.key == key && s.endpoint == endpoint)
        else {
            return false;
        };
        s.touched = Instant::now();
        if initialized {
            s.initialized = true;
        }
        initialized || s.initialized
    }
    fn remove(&self, id: &str) {
        self.inner.lock().expect("MCP sessions").remove(id);
    }
    fn upstreams(&self, id: &str) -> Option<Upstreams> {
        self.inner
            .lock()
            .expect("MCP sessions")
            .get(id)
            .map(|s| s.upstreams.clone())
    }
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp", any(aggregate))
        .route("/mcp/{server}", any(single))
}
async fn aggregate(State(st): State<AppState>, req: Request) -> Response {
    handle(st, String::new(), req).await
}
async fn single(State(st): State<AppState>, Path(server): Path<String>, req: Request) -> Response {
    handle(st, server, req).await
}
fn error(id: &Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}
fn reply(id: &Value, result: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
fn valid_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get("origin") else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    let Some(host) = h.get("host").and_then(|h| h.to_str().ok()) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && reqwest::Url::parse(&format!("{}://{host}", url.scheme()))
            .is_ok_and(|expected| expected.origin() == url.origin())
}
async fn handle(st: AppState, endpoint: String, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    if !valid_origin(&parts.headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let auth: KeyAuth = match crate::proxy::authenticate(&st, &parts.headers, false).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    if parts.method != Method::POST && parts.method != Method::DELETE {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if parts
        .headers
        .get("mcp-protocol-version")
        .is_some_and(|h| h != VERSION)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let session = parts
        .headers
        .get("mcp-session-id")
        .and_then(|h| h.to_str().ok());
    if parts.method == Method::DELETE {
        let Some(id) = session.filter(|id| st.mcp.check(id, auth.key_id, &endpoint, false)) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        st.mcp.remove(id);
        return StatusCode::NO_CONTENT.into_response();
    }
    if parts
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|h| {
            h.split(';')
                .next()
                .is_none_or(|h| h.trim() != "application/json")
        })
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(body, BODY_CAP).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return error(&Value::Null, -32700, "Parse error");
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = v.get("method").and_then(Value::as_str).filter(|_| {
        v["jsonrpc"] == "2.0"
            && (v.get("id").is_none() || id.is_i64() || id.is_u64() || id.is_string())
    }) else {
        return error(&Value::Null, -32600, "Invalid Request");
    };
    if v.get("params").is_some_and(|p| !p.is_object()) {
        return error(&id, -32602, "Parameters must be an object");
    }
    if method == "initialize" {
        if session.is_some()
            || !v["params"]["protocolVersion"].is_string()
            || !v["params"]["capabilities"].is_object()
            || !v["params"]["clientInfo"].is_object()
            || id.is_null()
        {
            return error(&id, -32602, "Invalid initialize parameters");
        }
        let Some(id_session) = st.mcp.create(auth.key_id, &endpoint) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let mut r = reply(
            &id,
            json!({"protocolVersion":VERSION,"capabilities":{"tools":{}},"serverInfo":{"name":"Tokomotive","version":env!("CARGO_PKG_VERSION")},"instructions":"Tools only. Resources, prompts and server requests are not supported."}),
        );
        r.headers_mut()
            .insert("mcp-session-id", id_session.parse().expect("session ID"));
        return r;
    }
    let initialized = method == "notifications/initialized";
    let Some(session) = session.filter(|s| st.mcp.check(s, auth.key_id, &endpoint, initialized))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }
    match method {
        "ping" => reply(&id, json!({})),
        "tools/list" | "tools/call" => {
            let Some(upstreams) = st.mcp.upstreams(session) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            tools(&st, &auth, &endpoint, upstreams, method, &id, &v["params"]).await
        }
        _ => error(&id, -32601, "Method not found"),
    }
}

async fn connection(
    upstreams: &Upstreams,
    id: i64,
) -> Option<Arc<tokio::sync::Mutex<upstream::Connection>>> {
    let mut entries = upstreams.lock().await;
    if entries.len() >= 64 && !entries.contains_key(&id) {
        return None;
    }
    Some(entries.entry(id).or_default().clone())
}
async fn tools(
    st: &AppState,
    auth: &KeyAuth,
    endpoint: &str,
    upstreams: Upstreams,
    method: &str,
    id: &Value,
    params: &Value,
) -> Response {
    let Ok(permissions) = data::Permissions::load(st, auth).await else {
        return error(id, -32603, "Access lookup failed");
    };
    let Ok(servers) = data::servers(&st.db).await else {
        return error(id, -32603, "Server lookup failed");
    };
    if servers.len() > 64 {
        return error(id, -32603, "Too many MCP servers");
    }
    let servers: Vec<_> = servers
        .into_iter()
        .filter(|s| s.enabled && (endpoint.is_empty() || s.name == endpoint))
        .collect();
    if method == "tools/list" {
        let mut tools = Vec::new();
        let mut bytes = 0;
        for server in servers.iter().filter(|s| permissions.server(&s.name)) {
            let Some(connection) = connection(&upstreams, server.id).await else {
                return error(
                    id,
                    -32603,
                    "Session upstream limit reached; initialize a new session",
                );
            };
            let Ok(mut upstream) = connection.try_lock() else {
                return error(id, -32603, "Upstream is busy; retry this request");
            };
            let Ok(list) = upstream.tools(st, server).await else {
                return error(id, -32603, "Upstream tool list failed");
            };
            for mut tool in list {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                if !permissions.permits(&server.name, name) {
                    continue;
                }
                let name = if endpoint.is_empty() {
                    format!("{}__{name}", server.name)
                } else {
                    name.to_owned()
                };
                if name.len() > 128 {
                    continue;
                }
                tool["name"] = json!(name);
                bytes += tool.to_string().len();
                if tools.len() >= upstream::MAX_TOOLS || bytes > upstream::JSON_CAP {
                    return error(id, -32603, "Tool list exceeds gateway limit");
                }
                tools.push(tool);
            }
        }
        tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let offset = match params.get("cursor") {
            None => 0,
            Some(Value::String(s)) if s.len() <= 8 => match s.parse::<usize>() {
                Ok(n) if n <= tools.len() => n,
                _ => return error(id, -32602, "Invalid cursor"),
            },
            _ => return error(id, -32602, "Invalid cursor"),
        };
        let end = (offset + 100).min(tools.len());
        let mut result = json!({"tools":&tools[offset..end]});
        if end < tools.len() {
            result["nextCursor"] = json!(end.to_string());
        }
        return reply(id, result);
    }
    let Some(name) = params["name"].as_str().filter(|s| data::valid_name(s, 128)) else {
        return error(id, -32602, "Invalid tool name");
    };
    let (server_name, tool) = if endpoint.is_empty() {
        match name.split_once("__") {
            Some(v) => v,
            None => return error(id, -32602, "Use a prefixed tool name"),
        }
    } else {
        (endpoint, name)
    };
    let start = Instant::now();
    // Write before dispatch: cancellation, shutdown or a failed final UPDATE
    // must not erase evidence that a tool might have had a side effect.
    let logged = sqlx::query("INSERT INTO mcp_calls(ts_ms,key_id,user_id,server,tool,status,latency_ms,error_class) VALUES(?,?,?,?,?,'unknown',0,'incomplete')")
        .bind(chrono::Utc::now().timestamp_millis()).bind(auth.key_id).bind(auth.user_id).bind(server_name).bind(tool).execute(&st.db).await;
    let Ok(logged) = logged else {
        return error(id, -32603, "Tool call log failed");
    };
    let log_id = logged.last_insert_rowid();
    let scopes = [
        (
            crate::ratelimit::Scope::Key(auth.key_id),
            crate::ratelimit::Limits {
                rpm: auth.key_limits.rpm,
                tpm: None,
            },
        ),
        (
            crate::ratelimit::Scope::User(auth.user_id),
            crate::ratelimit::Limits {
                rpm: auth.user_limits.rpm,
                tpm: None,
            },
        ),
    ];
    let mut class = "";
    let mut result = None;
    let mut value = None;
    if let Err(limited) = st
        .limiter
        .admit(&scopes, chrono::Utc::now().timestamp_millis())
    {
        class = "rate_limit";
        let mut response = error(id, -32000, "Rate limit exceeded");
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response.headers_mut().insert(
            "retry-after",
            limited.retry_after_secs.to_string().parse().expect("retry"),
        );
        result = Some(response);
    } else if !permissions.permits(server_name, tool) {
        class = "denied";
    } else if let Some(server) = servers.iter().find(|s| s.name == server_name) {
        if params.get("arguments").is_some_and(|a| !a.is_object()) {
            class = "invalid_arguments";
        } else {
            let connection = connection(&upstreams, server.id).await;
            let mut p = json!({"name":tool});
            if let Some(args) = params.get("arguments") {
                p["arguments"] = args.clone();
            }
            let requested = match connection.as_ref().map(|c| c.try_lock()) {
                Some(Ok(mut connection)) => connection.request(st, server, "tools/call", p).await,
                _ => Err(upstream::Failure("upstream_busy")),
            };
            match requested {
                Ok(mut v) => {
                    v["id"] = id.clone();
                    if v.get("error").is_some() {
                        class = "upstream_rpc";
                    } else if v["result"]["isError"] == true {
                        class = "tool_error";
                    }
                    value = Some(v);
                }
                Err(e) => class = e.0,
            }
        }
    } else {
        class = "not_found";
    }
    let status = if class.is_empty() { "ok" } else { "error" };
    // Arguments, results, upstream error messages and credentials are never logged.
    let logged = sqlx::query("UPDATE mcp_calls SET status=?,latency_ms=?,error_class=? WHERE id=?")
        .bind(status)
        .bind(start.elapsed().as_millis().min(i64::MAX as u128) as i64)
        .bind(class)
        .bind(log_id)
        .execute(&st.db)
        .await;
    if logged.is_err() {
        return error(id, -32603, "Tool call log failed");
    }
    metrics::counter!("tokomotive_mcp_tool_calls_total", "status" => status, "error_class" => class).increment(1);
    if let Some(result) = result {
        return result;
    }
    if let Some(v) = value {
        return Json(v).into_response();
    }
    if matches!(class, "denied" | "not_found" | "invalid_arguments") {
        error(id, -32602, "Tool not allowed or invalid")
    } else {
        error(id, -32603, "Upstream tool call failed")
    }
}
