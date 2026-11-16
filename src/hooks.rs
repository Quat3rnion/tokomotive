//! Signed external hooks. Diagnostics contain names and outcomes, never bodies.
use anyhow::{Result, ensure};
use axum::{
    Json,
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{db, state::AppState};

pub const RESPONSE_LIMIT: usize = 1024 * 1024;
pub const EVENT_QUEUE: usize = 64;
pub const EVENT_CONCURRENCY: usize = 4;

#[derive(Clone, sqlx::FromRow)]
pub struct Hook {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub pre_request: bool,
    pub post_response: bool,
    pub event: bool,
    pub include_bodies: bool,
    pub timeout_ms: i64,
    pub fail_closed: bool,
    pub enabled: bool,
    pub allow_private: bool,
    pub priority: i64,
    pub(crate) secret: String,
}
impl Hook {
    pub fn stage(&self, stage: &str) -> bool {
        match stage {
            "pre_request" => self.pre_request,
            "post_response" => self.post_response,
            "event" => self.event,
            _ => false,
        }
    }
}

pub async fn load(
    st: &AppState,
    route: Option<&str>,
    team: Option<i64>,
    key: i64,
) -> Result<Vec<Hook>> {
    Ok(sqlx::query_as("SELECT h.* FROM hooks h WHERE enabled=1 AND EXISTS(SELECT 1 FROM hook_attachments a WHERE a.hook_id=h.id AND (a.global=1 OR a.route=? OR a.team_id=? OR a.key_id=?)) ORDER BY priority,name LIMIT 64")
        .bind(route).bind(team).bind(key).fetch_all(&st.db).await?)
}

pub fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Continue,
    Modify { patch: Value },
    Reject { status: u16, message: String },
}

pub struct Call {
    pub decision: Option<Decision>,
    pub outcome: &'static str,
    pub ms: i64,
    pub status: Option<u16>,
}

/// The outer deadline includes DNS, connection, headers, and every response byte.
async fn invoke(st: &AppState, hook: &Hook, stage: &str, request_id: &str, data: Value) -> Call {
    let start = Instant::now();
    let mut http_status = None;
    let result = tokio::time::timeout(Duration::from_millis(hook.timeout_ms.clamp(1, 30000) as u64), async {
        let timestamp = chrono::Utc::now().timestamp();
        let body = serde_json::to_vec(&json!({"version":1,"stage":stage,"hook":hook.name,"request_id":request_id,"timestamp":timestamp,"data":data}))?;
        let secret = st.secret_key.decrypt(&hook.secret)?;
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())?;
        mac.update(format!("{timestamp}.").as_bytes());
        mac.update(&body);
        let signature = format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()));
        let url = reqwest::Url::parse(&hook.url)?;
        let client = crate::secrets::adapters::webhook_client(&url, hook.allow_private, &st.egress).await?;
        let mut response = client.post(url).header("content-type", "application/json")
            .header("x-tokomotive-hook", &hook.name)
            .header("x-tokomotive-signature", signature).body(body).send().await?;
        let status = response.status().as_u16();
        http_status = Some(status);
        ensure!(response.status().is_success(), "status");
        ensure!(response.content_length().is_none_or(|n| n <= RESPONSE_LIMIT as u64), "size");
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= RESPONSE_LIMIT, "size");
            bytes.extend_from_slice(&chunk);
        }
        let decision = if stage == "event" {
            Decision::Continue
        } else {
            let decision: Decision = serde_json::from_slice(&bytes)?;
            match &decision {
                Decision::Reject { status, message } => ensure!((400..500).contains(status) && message.len() <= 4096, "decision"),
                Decision::Modify { patch } => ensure!(patch.is_object(), "patch must be an object"),
                _ => {}
            }
            decision
        };
        Ok::<_, anyhow::Error>((decision, status))
    }).await;
    let (decision, mut outcome, status) = match result {
        Ok(Ok((decision, status))) => {
            let outcome = match decision {
                Decision::Continue => "continue",
                Decision::Modify { .. } => "modified",
                Decision::Reject { .. } => "rejected",
            };
            (Some(decision), outcome, Some(status))
        }
        Err(_) => (None, "timeout", http_status),
        // Do not log an error chain: HTTP errors can contain URL credentials.
        Ok(Err(_)) => (None, "error", http_status),
    };
    if decision.is_none() && hook.fail_closed && stage != "event" {
        outcome = if outcome == "timeout" {
            "rejected_timeout"
        } else {
            "rejected_error"
        };
    }
    let ms = start.elapsed().as_millis() as i64;
    Call {
        decision,
        outcome,
        ms,
        status,
    }
}
async fn record(st: &AppState, hook: &Hook, stage: &str, request_id: &str, call: &Call) {
    metrics::counter!("llm_hook_calls_total", "hook" => hook.name.clone(), "stage" => stage.to_owned(), "outcome" => call.outcome).increment(1);
    metrics::histogram!("llm_hook_duration_seconds", "hook" => hook.name.clone(), "stage" => stage.to_owned(), "outcome" => call.outcome).record(call.ms as f64 / 1000.0);
    if sqlx::query("INSERT INTO hook_calls(request_id,hook,stage,outcome,ms,status,ts_ms) VALUES(?,?,?,?,?,?,?)")
        .bind(request_id).bind(&hook.name).bind(stage).bind(call.outcome).bind(call.ms).bind(call.status).bind(db::now_ms()).execute(&st.db).await.is_err() {
        tracing::warn!("Hook diagnostics write failed");
    }
}
pub async fn call(st: &AppState, hook: &Hook, stage: &str, request_id: &str, data: Value) -> Call {
    let call = invoke(st, hook, stage, request_id, data).await;
    record(st, hook, stage, request_id, &call).await;
    call
}

/// RFC 7396: null removes an object member; arrays and scalars replace it.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(patch) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        let object = target.as_object_mut().expect("object");
        for (key, value) in patch {
            if value.is_null() {
                object.remove(key);
            } else {
                merge_patch(object.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[derive(Default)]
struct Data {
    context: Option<Arc<Context>>,
    pre_done: bool,
    pre_ran: BTreeSet<i64>,
    model_hopped: bool,
    body_rejected: bool,
    status: Option<u16>,
    response: Option<Value>,
    done: bool,
}
#[derive(Clone)]
struct Context {
    st: AppState,
    hooks: Vec<Hook>,
    request_id: String,
    request: Value,
}
#[derive(Clone, Default)]
pub(crate) struct Session(Arc<Mutex<Data>>, Arc<tokio::sync::Notify>);
impl Session {
    pub fn pre_done(&self) -> bool {
        self.0.lock().unwrap().pre_done
    }
    pub fn has_pending_pre(&self) -> bool {
        let state = self.0.lock().unwrap();
        state.context.as_ref().is_some_and(|c| {
            c.hooks
                .iter()
                .any(|h| h.pre_request && !state.pre_ran.contains(&h.id))
        })
    }
    pub fn model_hopped(&self) -> bool {
        self.0.lock().unwrap().model_hopped
    }
    pub fn mark_model_hop(&self) {
        self.0.lock().unwrap().model_hopped = true;
    }
    pub fn reject_body(&self) {
        self.0.lock().unwrap().body_rejected = true;
    }
    pub fn request_body(&self, body: &Value) {
        let mut state = self.0.lock().unwrap();
        if let Some(context) = &mut state.context {
            let context = Arc::make_mut(context);
            context.request["body"] = body.clone();
            context.request["model"] = body["model"].clone();
        }
    }
    pub async fn configure(
        &self,
        st: &AppState,
        route: Option<&str>,
        team: Option<i64>,
        key: i64,
        request: Value,
    ) -> Result<()> {
        let hooks = if self
            .context()
            .is_none_or(|c| c.request["route"] != request["route"])
        {
            load(st, route, team, key).await?
        } else {
            vec![]
        };
        let mut state = self.0.lock().unwrap();
        if let Some(context) = &mut state.context {
            let context = Arc::make_mut(context);
            for hook in hooks {
                if !context.hooks.iter().any(|h| h.id == hook.id) {
                    context.hooks.push(hook);
                }
            }
            context
                .hooks
                .sort_by(|a, b| (a.priority, &a.name).cmp(&(b.priority, &b.name)));
            context.request = request;
        } else {
            state.context = Some(Arc::new(Context {
                st: st.clone(),
                hooks,
                request_id: new_secret(),
                request,
            }));
        }
        Ok(())
    }
    fn context(&self) -> Option<Arc<Context>> {
        self.0.lock().unwrap().context.clone()
    }
    pub fn has_pre(&self) -> bool {
        self.context()
            .is_some_and(|c| c.hooks.iter().any(|h| h.pre_request))
    }
    pub fn has_post(&self) -> bool {
        self.context()
            .is_some_and(|c| c.hooks.iter().any(|h| h.post_response))
    }
    pub async fn pre(
        &self,
        body: &mut Value,
        guardrails: &crate::guardrails::Session,
        validate: impl Fn(&Value) -> Result<(), &'static str>,
    ) -> Result<(), Box<Response>> {
        self.0.lock().unwrap().pre_done = true;
        let Some(c) = self.context() else {
            return Ok(());
        };
        for hook in c.hooks.iter().filter(|h| h.pre_request) {
            if !self.0.lock().unwrap().pre_ran.insert(hook.id) {
                continue;
            }
            let mut data = c.request.clone();
            data["body"] = body.clone();
            data["model"] = body["model"].clone();
            let mut call = invoke(&c.st, hook, "pre_request", &c.request_id, data).await;
            if let Some(Decision::Modify { patch }) = &call.decision {
                let mut candidate = body.clone();
                merge_patch(&mut candidate, patch);
                if let Err(message) = validate(&candidate) {
                    call.outcome = "rejected";
                    call.decision = Some(Decision::Reject {
                        status: 400,
                        message: message.into(),
                    });
                }
            }
            record(&c.st, hook, "pre_request", &c.request_id, &call).await;
            match call.decision {
                Some(Decision::Modify { patch }) => merge_patch(body, &patch),
                Some(Decision::Reject { status, message }) => {
                    self.0.lock().unwrap().status = Some(status);
                    self.reject_body();
                    let message = guardrails
                        .hook_message(crate::guardrails::Direction::Request, &message)
                        .await;
                    return Err(Box::new(rejected(status, &hook.name, &message)));
                }
                None if hook.fail_closed => {
                    self.0.lock().unwrap().status = Some(502);
                    self.reject_body();
                    return Err(Box::new(rejected(502, &hook.name, "Hook unavailable.")));
                }
                _ => {}
            }
        }
        let mut state = self.0.lock().unwrap();
        let context = Arc::make_mut(state.context.as_mut().expect("context"));
        context.request["body"] = body.clone();
        context.request["model"] = body["model"].clone();
        Ok(())
    }
    pub fn status(&self) -> Option<u16> {
        self.0.lock().unwrap().status
    }
    pub fn complete(&self) {
        self.0.lock().unwrap().done = true;
        self.1.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let notified = self.1.notified();
            if self.0.lock().unwrap().done {
                return;
            }
            notified.await;
        }
    }
    /// Capture only explicit event bodies, after response guardrails. Stream
    /// capture drains on disconnect, with at most 1 MiB retained in memory.
    pub async fn capture(&self, response: Response) -> Response {
        use futures_util::StreamExt;
        if self
            .context()
            .is_none_or(|c| !c.hooks.iter().any(|h| h.event && h.include_bodies))
        {
            self.complete();
            return response;
        }
        let (mut parts, body) = response.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        let session = self.clone();
        tokio::spawn(async move {
            let mut stream = body.into_data_stream();
            let mut captured = Vec::new();
            let mut over = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        if !over && captured.len() + chunk.len() <= RESPONSE_LIMIT {
                            captured.extend_from_slice(&chunk);
                        } else {
                            over = true;
                            captured.clear();
                        }
                        let _ = tx.send(Ok(chunk)).await;
                    }
                    Err(_) => {
                        over = true;
                        let _ = tx
                            .send(Err(std::io::Error::other("Response read failed")))
                            .await;
                        break;
                    }
                }
            }
            if !over {
                session.0.lock().unwrap().response =
                    Some(serde_json::from_slice(&captured).unwrap_or_else(|_| {
                        Value::String(String::from_utf8_lossy(&captured).into_owned())
                    }));
            }
            session.complete();
        });
        Response::from_parts(
            parts,
            Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
    }
    pub async fn response(
        &self,
        response: Response,
        guardrails: &crate::guardrails::Session,
    ) -> Response {
        let Some(c) = self.context() else {
            return response;
        };
        if c.request["body"]["stream"] == true
            || c.request["endpoint"] == "audio_speech"
            || !c.hooks.iter().any(|h| h.post_response)
            || !response.status().is_success()
            || response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|s| !s.starts_with("application/json"))
        {
            return response;
        }
        let (mut parts, body) = response.into_parts();
        let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return rejected(502, "response", "Response exceeds the inspection limit."),
        };
        let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
            return Response::from_parts(parts, Body::from(bytes));
        };
        for hook in c.hooks.iter().filter(|h| h.post_response) {
            let data = json!({"request":c.request,"body":value,"status":parts.status.as_u16()});
            let call = call(&c.st, hook, "post_response", &c.request_id, data).await;
            match call.decision {
                Some(Decision::Modify { patch }) => merge_patch(&mut value, &patch),
                Some(Decision::Reject { status, message }) => {
                    self.0.lock().unwrap().status = Some(status);
                    self.reject_body();
                    let message = guardrails
                        .hook_message(crate::guardrails::Direction::Response, &message)
                        .await;
                    return rejected(status, &hook.name, &message);
                }
                None if hook.fail_closed => {
                    self.0.lock().unwrap().status = Some(502);
                    self.reject_body();
                    return rejected(502, &hook.name, "Hook unavailable.");
                }
                _ => {}
            }
        }
        parts.headers.remove(header::CONTENT_LENGTH);
        Response::from_parts(parts, Body::from(value.to_string()))
    }
    pub async fn ledger(&self, id: i64, entry: &db::NewEntry) {
        self.wait().await;
        let Some(c) = self.context() else {
            return;
        };
        if sqlx::query("UPDATE hook_calls SET ledger_id=? WHERE request_id=?")
            .bind(id)
            .bind(&c.request_id)
            .execute(&c.st.db)
            .await
            .is_err()
        {
            tracing::warn!("Hook Ledger link failed");
        }
        for hook in c.hooks.iter().filter(|h| h.event) {
            let mut data = json!({"ledger_id":id,"ledger":entry});
            if self.0.lock().unwrap().body_rejected {
                data["body_omitted"] = "rejected".into();
            } else if hook.include_bodies {
                data["request"] = if c.request["body"].to_string().len() <= RESPONSE_LIMIT {
                    c.request["body"].clone()
                } else {
                    Value::Null
                };
                data["response"] = self
                    .0
                    .lock()
                    .unwrap()
                    .response
                    .clone()
                    .unwrap_or(Value::Null);
                data["bodies_omitted_over_cap"] =
                    Value::Bool(data["request"].is_null() || data["response"].is_null());
            }
            c.st.hooks
                .enqueue(c.st.clone(), hook.clone(), c.request_id.clone(), id, data);
        }
    }
}
pub(crate) fn rejected(status: u16, hook: &str, message: &str) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), Json(json!({"error":{"type":"hook_error","code":"hook_rejected","message":message,"hook":hook}}))).into_response()
}

/// A bounded waiting queue plus a separate bound on active network calls.
#[derive(Clone)]
pub struct Events {
    slots: Arc<tokio::sync::Semaphore>,
    active: Arc<tokio::sync::Semaphore>,
}
impl Default for Events {
    fn default() -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(EVENT_QUEUE)),
            active: Arc::new(tokio::sync::Semaphore::new(EVENT_CONCURRENCY)),
        }
    }
}
impl Events {
    fn enqueue(&self, st: AppState, hook: Hook, request: String, id: i64, data: Value) {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            metrics::counter!("llm_hook_events_dropped_total", "hook" => hook.name).increment(1);
            return;
        };
        let active = self.active.clone();
        st.clone().spawn_tracked(async move {
            let Ok(_permit) = active.acquire_owned().await else {
                return;
            };
            drop(slot);
            call(&st, &hook, "event", &request, data).await;
            let _ = sqlx::query(
                "UPDATE hook_calls SET ledger_id=? WHERE request_id=? AND stage='event'",
            )
            .bind(id)
            .bind(request)
            .execute(&st.db)
            .await;
        });
    }
}
