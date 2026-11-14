use axum::{
    Json, Router,
    body::Body,
    extract::Form,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub type SeenRequest = (String, HeaderMap, Value);
pub struct MockCodex {
    pub base: String,
    pub requests: Arc<Mutex<Vec<SeenRequest>>>,
    pub tokens: Arc<Mutex<Vec<HashMap<String, String>>>>,
    pub unauthorized: Arc<Mutex<bool>>,
    pub poll: Arc<Mutex<(StatusCode, Value)>>,
    pub completion_gate: Arc<Mutex<Option<Arc<tokio::sync::Semaphore>>>>,
    pub body_started: Arc<tokio::sync::Notify>,
    pub reply: Arc<Mutex<Option<(StatusCode, HeaderMap, String)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockCodex {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockCodex {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let r = requests.clone();
        let tokens = Arc::new(Mutex::new(Vec::new()));
        let t = tokens.clone();
        let unauthorized = Arc::new(Mutex::new(false));
        let u = unauthorized.clone();
        let poll = Arc::new(Mutex::new((StatusCode::FORBIDDEN, json!({}))));
        let p = poll.clone();
        let completion_gate = Arc::new(Mutex::new(None::<Arc<tokio::sync::Semaphore>>));
        let gate = completion_gate.clone();
        let body_started = Arc::new(tokio::sync::Notify::new());
        let started = body_started.clone();
        let reply = Arc::new(Mutex::new(None::<(StatusCode, HeaderMap, String)>));
        let override_reply = reply.clone();
        let id=format!("h.{}.s",URL_SAFE_NO_PAD.encode(json!({"email":"owner@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"acct","chatgpt_plan_type":"plus"}}).to_string()));
        let app=Router::new()
            .route("/oauth/token",post(move |Form(form):Form<HashMap<String,String>>| {let (t,id)=(t.clone(),id.clone());async move {
                let refresh=form["grant_type"]=="refresh_token";t.lock().unwrap().push(form);
                Json(json!({"access_token":if refresh {"codex-refreshed"} else {"codex-access"},"refresh_token":"refresh-secret","id_token":id,"expires_in":3600}))
            }}))
            .route("/api/accounts/deviceauth/usercode",post(|| async {Json(json!({"device_auth_id":"device-secret","user_code":"ABCD-12345","interval":"1"}))}))
            .route("/api/accounts/deviceauth/token",post(move || {let p=p.clone();async move {let (s,v)=p.lock().unwrap().clone();(s,Json(v))}}))
            .route("/backend-api/codex/responses",post(move |uri:Uri,headers:HeaderMap,Json(v):Json<Value>| {let (r,u,gate,started,override_reply)=(r.clone(),u.clone(),gate.clone(),started.clone(),override_reply.clone());async move {
                r.lock().unwrap().push((uri.to_string(),headers,v));
                if std::mem::take(&mut *u.lock().unwrap()) {return StatusCode::UNAUTHORIZED.into_response();}
                if let Some((status, headers, body)) = override_reply.lock().unwrap().clone() {
                    return (status, headers, body).into_response();
                }
                let events=[json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}),
                    json!({"type":"response.output_text.delta","delta":"Hello"}),
                    json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt-test","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Hello"}]}],"usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}})];
                let body:String=events.iter().map(|v|format!("data: {v}\n\n")).collect();
                let gate = gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(2);
                    tokio::spawn(async move {
                        let _ = tx.send(Ok(": collecting\n\n".into())).await;
                        started.notify_one();
                        let _permit = gate.acquire().await.unwrap();
                        let _ = tx.send(Ok(body)).await;
                    });
                    return Response::builder().header("content-type","text/event-stream").body(Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))).unwrap();
                }
                Response::builder().header("content-type","text/event-stream").body(Body::from(body)).unwrap()
            }}))
            .fallback(any(|| async {StatusCode::NOT_FOUND}));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            requests,
            tokens,
            unauthorized,
            poll,
            completion_gate,
            body_started,
            reply,
            task,
        }
    }
}
