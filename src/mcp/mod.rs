//! MCP Streamable HTTP gateway. The transport is deliberately POST-only.
//! Spec: https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
//! Lifecycle: https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle
use crate::{db::KeyAuth, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{Method, StatusCode},
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
type Upstreams = Arc<SessionTools>;
#[derive(Default)]
struct SessionTools {
    connections: tokio::sync::Mutex<HashMap<i64, Arc<tokio::sync::Mutex<upstream::Connection>>>>,
    names: Mutex<HashMap<String, (String, String)>>,
}

pub const VERSION: &str = "2025-06-18";
const BODY_CAP: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(15);
const MAX_REQUESTS: usize = 256;
const MAX_KEY_REQUESTS: usize = 8;
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
    requests: tokio::sync::Semaphore,
    key_requests: Mutex<HashMap<i64, usize>>,
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
            requests: tokio::sync::Semaphore::new(MAX_REQUESTS),
            key_requests: Mutex::new(HashMap::new()),
            ttl: ttl.min(SESSION_TTL),
            max: max.min(MAX_SESSIONS),
            per_key: per_key.min(MAX_KEY_SESSIONS),
        }
    }
    fn admit(&self, key: i64) -> Result<RequestPermit<'_>, StatusCode> {
        let permit = self
            .requests
            .try_acquire()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut keys = self.key_requests.lock().expect("MCP requests");
        let count = keys.entry(key).or_default();
        if *count >= MAX_KEY_REQUESTS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        *count += 1;
        Ok(RequestPermit {
            _global: permit,
            keys: &self.key_requests,
            key,
        })
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
struct RequestPermit<'a> {
    _global: tokio::sync::SemaphorePermit<'a>,
    keys: &'a Mutex<HashMap<i64, usize>>,
    key: i64,
}
impl Drop for RequestPermit<'_> {
    fn drop(&mut self) {
        let mut keys = self.keys.lock().expect("MCP requests");
        let count = keys.get_mut(&self.key).expect("admitted MCP key");
        *count -= 1;
        if *count == 0 {
            keys.remove(&self.key);
        }
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
async fn handle(st: AppState, endpoint: String, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // Native clients only. Comparing Origin with an untrusted Host header
    // does not provide a trusted-host allowlist against DNS rebinding.
    // Browser support needs a separate, explicit Operator opt-in policy.
    if parts.headers.contains_key("origin") {
        let mut response = error(
            &Value::Null,
            -32000,
            "Browser MCP clients are not supported",
        );
        *response.status_mut() = StatusCode::FORBIDDEN;
        return response;
    }
    let auth: KeyAuth = match crate::proxy::authenticate(&st, &parts.headers, false).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    // Hold admission through dispatch, not just the body read. No wait queue.
    let _permit = match st.mcp.admit(auth.key_id) {
        Ok(permit) => permit,
        Err(status) => return status.into_response(),
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
    let bytes =
        match tokio::time::timeout(BODY_DEADLINE, axum::body::to_bytes(body, BODY_CAP)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
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
    let mut entries = upstreams.connections.lock().await;
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
    if method == "tools/list" {
        // A failed refresh must not leave an earlier catalog usable.
        upstreams.names.lock().expect("MCP tool names").clear();
    }
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
        let mut names = HashMap::new();
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
            let list = match upstream.tools(st, server).await {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(
                        server_id = server.id,
                        error_class = e.0,
                        "MCP tool list rejected"
                    );
                    return error(id, -32603, "Upstream tool list failed");
                }
            };
            for mut tool in list {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                if !permissions.permits(&server.name, name) {
                    continue;
                }
                let tool_name = name.to_owned();
                let name = if endpoint.is_empty() {
                    format!("{}__{name}", server.name)
                } else {
                    name.to_owned()
                };
                if name.len() > 128 {
                    continue;
                }
                if names
                    .insert(name.clone(), (server.name.clone(), tool_name))
                    .is_some()
                {
                    tracing::warn!(
                        error_class = "aggregate_name_collision",
                        "MCP tool list rejected"
                    );
                    return error(id, -32603, "Duplicate gateway tool name");
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
        *upstreams.names.lock().expect("MCP tool names") = names;
        return reply(id, result);
    }
    let Some(name) = params["name"].as_str().filter(|s| data::valid_name(s, 128)) else {
        return error(id, -32602, "Invalid tool name");
    };
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
    if let Err(limited) = st
        .limiter
        .admit(&scopes, chrono::Utc::now().timestamp_millis())
    {
        rejected("rate_limit");
        let mut response = error(id, -32000, "Rate limit exceeded");
        *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
        response.headers_mut().insert(
            "retry-after",
            limited.retry_after_secs.to_string().parse().expect("retry"),
        );
        return response;
    }
    let route = upstreams
        .names
        .lock()
        .expect("MCP tool names")
        .get(name)
        .cloned();
    let Some((server_name, tool)) = route else {
        rejected("not_listed");
        return error(id, -32602, "List tools before calling a listed tool");
    };
    if !permissions.permits(&server_name, &tool) {
        rejected("denied");
        return error(id, -32602, "Tool not allowed or invalid");
    }
    let Some(server) = servers.iter().find(|s| s.name == server_name) else {
        rejected("not_found");
        return error(id, -32602, "Tool not allowed or invalid");
    };
    if params.get("arguments").is_some_and(|a| !a.is_object()) {
        rejected("invalid_arguments");
        return error(id, -32602, "Tool not allowed or invalid");
    }
    let start = Instant::now();
    // Only admitted calls get a durable row, before any possible side effect.
    let logged = sqlx::query("INSERT INTO mcp_calls(ts_ms,key_id,user_id,server,tool,status,latency_ms,error_class) VALUES(?,?,?,?,?,'unknown',0,'incomplete')")
        .bind(chrono::Utc::now().timestamp_millis()).bind(auth.key_id).bind(auth.user_id).bind(&server_name).bind(&tool).execute(&st.db).await;
    let Ok(logged) = logged else {
        return error(id, -32603, "Tool call log failed");
    };
    let log_id = logged.last_insert_rowid();
    let mut class = "";
    let mut value = None;
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
    if let Some(v) = value {
        return Json(v).into_response();
    }
    error(id, -32603, "Upstream tool call failed")
}

fn rejected(class: &'static str) {
    metrics::counter!("tokomotive_mcp_tool_calls_total", "status" => "rejected", "error_class" => class).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_admission_has_global_and_key_bounds_and_releases_idle_keys() {
        let sessions = Sessions::default();
        let mut permits = Vec::new();
        for key in 0..32 {
            for _ in 0..MAX_KEY_REQUESTS {
                permits.push(sessions.admit(key).unwrap());
            }
            if key == 0 {
                assert!(matches!(
                    sessions.admit(key),
                    Err(StatusCode::TOO_MANY_REQUESTS)
                ));
            }
        }
        assert!(matches!(
            sessions.admit(33),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        ));
        assert_eq!(sessions.key_requests.lock().unwrap().len(), 32);
        permits.clear();
        assert!(sessions.key_requests.lock().unwrap().is_empty());
        assert!(sessions.admit(33).is_ok());
    }
}
