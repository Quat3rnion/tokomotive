//! Owned OpenAI files and batches. Object credentials never use pool fallback.
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    db::{self, KeyAuth},
    provider::{self, ProviderAuth, ProviderKind},
    proxy,
    state::{AppState, Provider},
};

const JSON_LIMIT: usize = 1024 * 1024;

/// The generic proxy must not bypass ownership through encoded API paths.
pub(crate) fn reserved_path(path: &str) -> bool {
    let mut decoded = path.as_bytes().to_vec();
    for _ in 0..8 {
        let mut next = Vec::with_capacity(decoded.len());
        let mut i = 0;
        while i < decoded.len() {
            if decoded[i] == b'%'
                && i + 2 < decoded.len()
                && let (Some(a), Some(b)) = (
                    (decoded[i + 1] as char).to_digit(16),
                    (decoded[i + 2] as char).to_digit(16),
                )
            {
                next.push((a * 16 + b) as u8);
                i += 3;
            } else {
                next.push(decoded[i]);
                i += 1;
            }
        }
        if next == decoded {
            let mut segments = next
                .split(|b| *b == b'/' || *b == b'\\')
                .filter(|s| !s.is_empty());
            return segments
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case(b"v1"))
                && segments.next().is_some_and(|s| {
                    s.eq_ignore_ascii_case(b"files") || s.eq_ignore_ascii_case(b"batches")
                });
        }
        decoded = next;
    }
    true // Excessive encoding is not a supported generic API path.
}

struct ApiError(Box<Response>);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        *self.0
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(_: anyhow::Error) -> Self {
        tracing::error!("files/batches request failed");
        error_response(
            StatusCode::BAD_GATEWAY,
            "Upstream or storage operation failed.",
        )
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}
fn error_response(status: StatusCode, message: &str) -> ApiError {
    let kind = match status {
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::PAYMENT_REQUIRED => "budget_exceeded",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        s if s.is_server_error() => "api_error",
        _ => "invalid_request_error",
    };
    ApiError(Box::new(
        (
            status,
            Json(json!({"error":{"type":kind,"message":message}})),
        )
            .into_response(),
    ))
}
fn bad(message: &str) -> ApiError {
    error_response(StatusCode::BAD_REQUEST, message)
}
fn missing() -> ApiError {
    error_response(StatusCode::NOT_FOUND, "Object not found.")
}

#[derive(Clone, sqlx::FromRow)]
struct Object {
    kind: String,
    id: String,
    key_id: i64,
    provider: String,
    provider_key: i64,
    credential_hash: String,
    base_url: String,
    created_at: i64,
    status: String,
    object_json: String,
    manifest_json: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn selected(st: &AppState, auth: &KeyAuth, headers: &HeaderMap) -> Result<Provider, ApiError> {
    let name = headers
        .get("x-tokomotive-provider")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| bad("Set x-tokomotive-provider to an OpenAI API-key Provider."))?;
    let p = st
        .router_table()
        .providers
        .iter()
        .find(|p| p.name == name)
        .cloned()
        .ok_or_else(|| bad("Unknown Provider."))?;
    permitted(auth, &p)?;
    if p.keys.is_empty() {
        return Err(bad("Provider has no API key."));
    }
    Ok(p)
}
fn permitted(auth: &KeyAuth, p: &Provider) -> Result<(), ApiError> {
    if p.kind != ProviderKind::Openai
        || p.auth != ProviderAuth::ApiKey
        || !proxy::provider_allowed(auth, p)
    {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "Provider Access does not permit this operation.",
        ));
    }
    Ok(())
}
fn pinned(
    st: &AppState,
    o: &Object,
    auth: Option<&KeyAuth>,
) -> Result<(Provider, String), ApiError> {
    let p = st
        .router_table()
        .providers
        .iter()
        .find(|p| p.name == o.provider)
        .cloned()
        .ok_or_else(|| bad("The stored Provider is unavailable."))?;
    if let Some(auth) = auth {
        permitted(auth, &p)?;
    }
    if p.kind != ProviderKind::Openai || p.auth != ProviderAuth::ApiKey || p.base_url != o.base_url
    {
        return Err(bad("The stored Provider has changed."));
    }
    let key = p
        .keys
        .iter()
        .find(|k| db::hash_key(k) == o.credential_hash)
        .cloned()
        .ok_or_else(|| {
            bad("The stored Provider Key is unavailable. Restore it to access this object.")
        })?;
    Ok((p, key))
}
async fn request(
    st: &AppState,
    p: &Provider,
    key: &str,
    method: Method,
    path: &str,
) -> Result<reqwest::RequestBuilder, ApiError> {
    Ok(p.key_header.apply(
        st.snapshot_http(p)
            .await?
            .request(method, provider::upstream_url(&p.base_url, path)),
        key,
    ))
}
async fn send_json(
    provider: &str,
    request: reqwest::RequestBuilder,
) -> Result<(StatusCode, Value), ApiError> {
    let response = request
        .send()
        .await
        .map_err(|e| crate::egress::request_error(provider, e))?;
    let status = response.status();
    let body = proxy::read_capped(response, JSON_LIMIT, provider).await?;
    let value = serde_json::from_slice(&body).map_err(|_| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "Upstream did not return a JSON object.",
        )
    })?;
    Ok((status, value))
}
async fn own(st: &AppState, auth: &KeyAuth, kind: &str, id: &str) -> Result<Object, ApiError> {
    if !valid_id(id) {
        return Err(missing());
    }
    sqlx::query_as(
        "SELECT * FROM proxy_objects WHERE kind=? AND id=? AND key_id=? AND status!='deleted'",
    )
    .bind(kind)
    .bind(id)
    .bind(auth.key_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(missing)
}
async fn store(st: &AppState, o: &Object) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO proxy_objects(kind,id,key_id,provider,provider_key,credential_hash,base_url,created_at,status,object_json,manifest_json) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
        .bind(&o.kind).bind(&o.id).bind(o.key_id).bind(&o.provider).bind(o.provider_key).bind(&o.credential_hash).bind(&o.base_url).bind(o.created_at).bind(&o.status).bind(&o.object_json).bind(&o.manifest_json).execute(&st.db).await?;
    Ok(())
}
async fn update(st: &AppState, o: &Object, value: &Value) -> Result<(), ApiError> {
    if value.get("id").and_then(Value::as_str) != Some(&o.id) {
        return Err(error_response(
            StatusCode::BAD_GATEWAY,
            "Upstream object ID changed.",
        ));
    }
    sqlx::query(
        "UPDATE proxy_objects SET object_json=?,status=? WHERE kind=? AND id=? AND key_id=?",
    )
    .bind(value.to_string())
    .bind(value["status"].as_str().unwrap_or("available"))
    .bind(&o.kind)
    .bind(&o.id)
    .bind(o.key_id)
    .execute(&st.db)
    .await?;
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<String>,
    limit: Option<i64>,
    purpose: Option<String>,
}
async fn list(
    st: &AppState,
    auth: &KeyAuth,
    kind: &str,
    query: Option<&str>,
) -> Result<Response, ApiError> {
    admit(st, auth)?;
    let q: ListQuery = serde_urlencoded::from_str(query.unwrap_or(""))
        .map_err(|_| bad("Use after and limit (1 to 100)."))?;
    let limit = q.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(bad("Use a limit from 1 to 100."));
    }
    let cursor = match q.after {
        Some(id) => {
            let o = own(st, auth, kind, &id).await?;
            (o.created_at, o.id)
        }
        None => (i64::MAX, String::new()),
    };
    let rows:Vec<String>=sqlx::query_scalar("SELECT object_json FROM proxy_objects WHERE kind=? AND key_id=? AND status!='deleted' AND (created_at<? OR (created_at=? AND id<?)) AND (? IS NULL OR json_extract(object_json,'$.purpose')=?) ORDER BY created_at DESC,id DESC LIMIT ?")
        .bind(kind).bind(auth.key_id).bind(cursor.0).bind(cursor.0).bind(cursor.1).bind(&q.purpose).bind(&q.purpose).bind(limit+1).fetch_all(&st.db).await?;
    let has_more = rows.len() > limit as usize;
    let data: Vec<Value> = rows
        .iter()
        .take(limit as usize)
        .map(|o| serde_json::from_str(o))
        .collect::<Result<_, _>>()?;
    Ok(Json(json!({"object":"list","first_id":data.first().map(|v|&v["id"]),"last_id":data.last().map(|v|&v["id"]),"has_more":has_more,"data":data})).into_response())
}

async fn upload(
    st: &AppState,
    auth: &KeyAuth,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let p = selected(st, auth, headers)?;
    admit(st, auth)?;
    let boundary = multer::parse_boundary(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )
    .map_err(|_| bad("Use multipart/form-data."))?;
    let bytes = to_bytes(
        body,
        st.settings().file_upload_limit_mib.clamp(1, 63) * 1024 * 1024,
    )
    .await
    .map_err(|_| {
        error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "File upload exceeds the configured limit.",
        )
    })?;
    let stream = futures_util::stream::once(async { Ok::<_, std::convert::Infallible>(bytes) });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let (mut purpose, mut file) = (None, None);
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad("Invalid multipart body."))?
    {
        match field.name() {
            Some("purpose") if purpose.is_none() => {
                let text = field.text().await.map_err(|_| bad("Invalid purpose."))?;
                if text.is_empty() || text.len() > 128 {
                    return Err(bad("Purpose must have 1 to 128 bytes."));
                }
                purpose = Some(text);
            }
            Some("file") if file.is_none() => {
                let name = field.file_name().unwrap_or("input.jsonl").to_owned();
                let data = field.bytes().await.map_err(|_| bad("Invalid file."))?;
                if name.len() > 255 || data.is_empty() {
                    return Err(bad("Invalid or empty file."));
                }
                file = Some((name, data));
            }
            _ => return Err(bad("Send exactly one purpose field and one file field.")),
        }
    }
    let purpose = purpose.ok_or_else(|| bad("Missing purpose."))?;
    let (name, data) = file.ok_or_else(|| bad("Missing file."))?;
    let manifest = if purpose == "batch" {
        Some(validate_input(st, auth, &p, &data)?)
    } else {
        None
    };
    let form = reqwest::multipart::Form::new()
        .text("purpose", purpose.clone())
        .part(
            "file",
            reqwest::multipart::Part::bytes(data.to_vec()).file_name(name),
        );
    let (status, value) = send_json(
        &p.name,
        request(st, &p, &p.keys[0], Method::POST, "/v1/files")
            .await?
            .multipart(form),
    )
    .await?;
    if status.is_success() {
        let id = value["id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| {
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "Upstream returned an invalid file ID.",
                )
            })?;
        store(
            st,
            &Object {
                kind: "file".into(),
                id: id.into(),
                key_id: auth.key_id,
                provider: p.name.clone(),
                provider_key: 1,
                credential_hash: db::hash_key(&p.keys[0]),
                base_url: p.base_url.clone(),
                created_at: db::now_s(),
                status: "available".into(),
                object_json: value.to_string(),
                manifest_json: manifest.map(|m| serde_json::to_string(&m)).transpose()?,
            },
        )
        .await?;
    }
    Ok((status, Json(value)).into_response())
}

// The validated manifest, not the input contents, is retained locally.
#[derive(Clone, Serialize, Deserialize)]
struct InputLine {
    custom_id: String,
    model: String,
    endpoint: String,
    input_bytes: i64,
    max_output: i64,
}
fn validate_input(
    st: &AppState,
    auth: &KeyAuth,
    p: &Provider,
    data: &[u8],
) -> Result<Vec<InputLine>, ApiError> {
    let mut lines = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for line in data
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
    {
        if line.len() > JSON_LIMIT || lines.len() >= 10_000 {
            return Err(bad("Batch input exceeds 10,000 lines or 1 MiB per line."));
        }
        let v: Value =
            serde_json::from_slice(line).map_err(|_| bad("Batch input must be JSONL."))?;
        let model = v["body"]["model"]
            .as_str()
            .ok_or_else(|| bad("Each batch line needs body.model."))?;
        let catalog = st.catalog();
        if !catalog.allowed(&auth.allowed_models, Some(model)) {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Batch input contains a disallowed Model.",
            ));
        }
        if catalog.provider_for(model) != Some(p.name.as_str()) {
            return Err(bad(
                "Use catalog Model names from the selected Provider; Routes and model rewrite are not supported in batch files.",
            ));
        }
        let custom_id = v["custom_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 200)
            .ok_or_else(|| bad("Each batch line needs a unique custom_id (1 to 200 bytes)."))?;
        if !ids.insert(custom_id.to_owned()) {
            return Err(bad("Duplicate custom_id."));
        }
        let endpoint = v["url"].as_str().unwrap_or("");
        if v["method"] != "POST"
            || !matches!(
                endpoint,
                "/v1/chat/completions" | "/v1/responses" | "/v1/embeddings"
            )
            || v["body"]["stream"] == true
            || v["body"]["background"] == true
        {
            return Err(bad(
                "Use non-streaming POST chat/completions, responses or embeddings batch lines.",
            ));
        }
        let limits = ["max_tokens", "max_completion_tokens", "max_output_tokens"];
        let set: Vec<_> = limits
            .into_iter()
            .filter(|k| v["body"].get(k).is_some())
            .collect();
        let max_output = if endpoint.ends_with("embeddings") {
            0
        } else {
            if set.len() != 1
                || (endpoint.ends_with("responses") && set[0] != "max_output_tokens")
                || (endpoint.ends_with("completions") && set[0] == "max_output_tokens")
            {
                return Err(bad(
                    "Set one output limit: max_output_tokens for Responses, max_tokens or max_completion_tokens for Chat.",
                ));
            }
            v["body"][set[0]]
                .as_i64()
                .filter(|n| (1..=1_000_000).contains(n))
                .ok_or_else(|| bad("Output token limits must be from 1 to 1,000,000."))?
        };
        if contains_external_input(&v["body"])
            || v["body"].get("n").is_some_and(|n| n != 1)
            || v["body"].get("best_of").is_some_and(|n| n != 1)
            || v["body"]
                .get("modalities")
                .is_some_and(|v| v != &json!(["text"]))
            || v["body"].get("tools").is_some_and(|v| {
                v.as_array()
                    .is_none_or(|tools| tools.iter().any(|t| t["type"] != "function"))
            })
        {
            return Err(bad(
                "Batch inputs must be text only, with at most one output choice, function tools only, and no stored or external input.",
            ));
        }
        lines.push(InputLine {
            custom_id: custom_id.into(),
            model: model.into(),
            endpoint: endpoint.into(),
            input_bytes: line.len() as i64,
            max_output,
        });
    }
    if lines.is_empty() {
        return Err(bad("Batch input is empty."));
    }
    Ok(lines)
}
fn contains_external_input(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(k, v)| {
            matches!(
                k.as_str(),
                "image_url"
                    | "file_id"
                    | "file_url"
                    | "file_data"
                    | "input_audio"
                    | "audio_url"
                    | "audio"
                    | "previous_response_id"
                    | "conversation"
            ) || matches!(
                v.as_str(),
                Some(
                    "input_image"
                        | "input_file"
                        | "computer_use_preview"
                        | "web_search_preview"
                        | "web_search"
                        | "file_search"
                        | "code_interpreter"
                )
            ) || contains_external_input(v)
        }),
        Value::Array(a) => a.iter().any(contains_external_input),
        _ => false,
    }
}

pub async fn handle(State(st): State<AppState>, req: Request) -> Response {
    match handle_inner(st, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}
async fn handle_inner(st: AppState, req: Request) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let auth = proxy::authenticate(&st, &parts.headers, false)
        .await
        .map_err(ApiError)?;
    let segments: Vec<_> = parts
        .uri
        .path()
        .trim_start_matches('/')
        .split('/')
        .collect();
    let kind = if segments[1] == "files" {
        "file"
    } else {
        "batch"
    };
    if segments.len() == 2 {
        return match parts.method {
            Method::GET => list(&st, &auth, kind, parts.uri.query()).await,
            Method::POST if kind == "file" => upload(&st, &auth, &parts.headers, body).await,
            Method::POST => create(&st, &auth, body).await,
            _ => Err(error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "Method not allowed.",
            )),
        };
    }
    let o = own(&st, &auth, kind, segments[2]).await?;
    let (p, key) = pinned(&st, &o, Some(&auth))?;
    if kind == "batch"
        && ((parts.method == Method::GET && segments.len() == 3)
            || (parts.method == Method::POST
                && segments.len() == 4
                && segments.get(3) == Some(&"cancel")))
    {
        admit(&st, &auth)?;
        let (status, value) = send_json(
            &p.name,
            request(&st, &p, &key, parts.method, parts.uri.path()).await?,
        )
        .await?;
        if status.is_success() {
            observe(&st, &o, &value).await?;
        }
        return Ok((status, Json(value)).into_response());
    }
    if kind == "file"
        && parts.method == Method::GET
        && segments.len() == 4
        && segments.get(3) == Some(&"content")
    {
        admit(&st, &auth)?;
        let r = request(&st, &p, &key, Method::GET, parts.uri.path())
            .await?
            .send()
            .await
            .map_err(|e| crate::egress::request_error(&p.name, e))?;
        let mut response = Response::builder().status(r.status());
        if let Some(content_type) = r.headers().get("content-type") {
            response = response.header("content-type", content_type);
        }
        let provider = p.name.clone();
        return Ok(response
            .body(Body::from_stream(r.bytes_stream().map(move |chunk| {
                chunk.map_err(|e| crate::egress::request_error(&provider, e))
            })))
            .expect("valid response"));
    }
    if segments.len() != 3
        || !matches!(parts.method, Method::GET | Method::DELETE)
        || (kind == "batch" && parts.method == Method::DELETE)
    {
        return Err(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method not allowed.",
        ));
    }
    admit(&st, &auth)?;
    if parts.method == Method::DELETE && kind == "file" {
        let needed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM batch_jobs j JOIN proxy_objects b ON b.kind='batch' AND b.id=j.id WHERE j.accounted=0 AND j.key_id=? AND (json_extract(b.object_json,'$.input_file_id')=? OR json_extract(b.object_json,'$.output_file_id')=? OR json_extract(b.object_json,'$.error_file_id')=?))")
            .bind(auth.key_id).bind(&o.id).bind(&o.id).bind(&o.id).fetch_one(&st.db).await?;
        if needed {
            return Err(error_response(
                StatusCode::CONFLICT,
                "This file is required by an unaccounted batch.",
            ));
        }
    }
    let (status, value) = send_json(
        &p.name,
        request(&st, &p, &key, parts.method.clone(), parts.uri.path()).await?,
    )
    .await?;
    if status.is_success() {
        if parts.method == Method::DELETE {
            sqlx::query(
                "UPDATE proxy_objects SET status='deleted' WHERE kind=? AND id=? AND key_id=?",
            )
            .bind(kind)
            .bind(&o.id)
            .bind(auth.key_id)
            .execute(&st.db)
            .await?;
        } else {
            update(&st, &o, &value).await?;
        }
    }
    Ok((status, Json(value)).into_response())
}

fn admit(st: &AppState, auth: &KeyAuth) -> Result<(), ApiError> {
    if st.settings().mode == "simple" {
        return Ok(());
    }
    use crate::ratelimit::Scope;
    st.limiter
        .admit(
            &[
                (Scope::Key(auth.key_id), auth.key_limits),
                (Scope::User(auth.user_id), auth.user_limits),
            ],
            db::now_ms(),
        )
        .map_err(|limited| {
            let mut e = error_response(StatusCode::TOO_MANY_REQUESTS, "Rate Limit exceeded.");
            e.0.headers_mut()
                .insert("retry-after", limited.retry_after_secs.into());
            e
        })
}

#[derive(Serialize, Deserialize)]
struct Frozen {
    quote: crate::catalog::Quote,
    cost: crate::pricing::CardRates,
    price: crate::pricing::CardRates,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    lines: Vec<InputLine>,
    models: std::collections::HashMap<String, Frozen>,
}
#[derive(sqlx::FromRow)]
struct Job {
    key_id: i64,
    user_id: i64,
    team_id: Option<i64>,
    snapshot_json: String,
    simple: bool,
    user_name: String,
}
async fn create(st: &AppState, auth: &KeyAuth, body: Body) -> Result<Response, ApiError> {
    let bytes = to_bytes(body, JSON_LIMIT).await.map_err(|_| {
        error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Batch create limit is 1 MiB.",
        )
    })?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| bad("Invalid JSON."))?;
    let id = v["input_file_id"]
        .as_str()
        .ok_or_else(|| bad("Set input_file_id."))?;
    let o = own(st, auth, "file", id).await?;
    let (p, key) = pinned(st, &o, Some(auth))?;
    let lines: Vec<InputLine> = serde_json::from_str(
        o.manifest_json
            .as_deref()
            .ok_or_else(|| bad("Upload the input file with purpose=batch."))?,
    )?;
    if v["completion_window"] != "24h"
        || v.as_object().is_none_or(|m| {
            m.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "input_file_id" | "endpoint" | "completion_window" | "metadata"
                )
            })
        })
    {
        return Err(bad(
            "Use completion_window=24h, input_file_id, endpoint and optional metadata.",
        ));
    }
    let catalog = st.catalog();
    let simple = st.settings().mode == "simple";
    let mut models = std::collections::HashMap::new();
    let mut held = 0i64;
    for line in &lines {
        if !catalog.allowed(&auth.allowed_models, Some(&line.model)) {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "Batch input contains a disallowed Model.",
            ));
        }
        if v["endpoint"] != line.endpoint
            || catalog.provider_for(&line.model) != Some(p.name.as_str())
        {
            return Err(bad(
                "Batch endpoint and Model Provider must match every input line.",
            ));
        }
        if !models.contains_key(&line.model) {
            let entry = catalog
                .resolve(&line.model)
                .ok_or_else(|| bad("Unknown Model."))?;
            let cost = entry
                .cost
                .as_ref()
                .map(|(_, c)| c.clone())
                .unwrap_or_default();
            if !simple && entry.cost.is_none() {
                return Err(bad("Batch Models need a Cost Card in Full Mode."));
            }
            let price = auth
                .team_id
                .and_then(|t| entry.team_price.get(&t))
                .or(entry.price.as_ref())
                .map(|(_, spec)| spec.resolve(&cost))
                .unwrap_or_else(|| cost.marked_up(catalog.default_markup_bp));
            models.insert(
                line.model.clone(),
                Frozen {
                    quote: catalog.quote(
                        Some(&line.model),
                        auth.team_id,
                        &crate::usage::Usage::default(),
                    ),
                    cost,
                    price,
                },
            );
        }
        if !simple {
            let price = &models[&line.model].price;
            let input = std::iter::once(&price.base)
                .chain(price.tiers.iter().map(|t| &t.rates))
                .map(|r| {
                    [
                        r.input,
                        r.cache_read,
                        r.cache_write_5m,
                        r.cache_write_1h,
                        r.image_input,
                    ]
                    .into_iter()
                    .max()
                    .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
                .max(0);
            let output = std::iter::once(&price.base)
                .chain(price.tiers.iter().map(|t| &t.rates))
                .map(|r| r.output.max(r.image_output))
                .max()
                .unwrap_or(0)
                .max(0);
            let amount = ((line.input_bytes as i128 * input as i128
                + line.max_output as i128 * output as i128)
                * price.batch_multiplier_bp.unwrap_or(10_000) as i128
                + 9_999_999_999)
                / 10_000_000_000;
            held = held
                .checked_add(
                    i64::try_from(amount).map_err(|_| bad("Batch estimate is too large."))?,
                )
                .ok_or_else(|| bad("Batch estimate is too large."))?;
        }
    }
    admit(st, auth)?;
    let snapshot = serde_json::to_string(&Snapshot { lines, models })?;
    let intent = format!("pending-{:032x}", rand::random::<u128>());
    {
        let _guard = st.budget_check.lock().await;
        if !simple {
            let budgets = crate::budgets::applicable(&st.db, auth, db::now_s()).await?;
            if budgets.is_empty() {
                return Err(bad(
                    "Batch create needs an enabled User, Team or Virtual Key Budget in Full Mode.",
                ));
            }
            let _reservation = st.reserve_budgets(&budgets, held).ok_or_else(|| {
                error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    "Batch estimate exceeds Budget headroom.",
                )
            })?;
        }
        sqlx::query("INSERT INTO batch_jobs(id,key_id,user_id,team_id,held_micros,snapshot_json,simple,created_at,status) VALUES(?,?,?,?,?,?,?,?, 'creating')")
            .bind(&intent).bind(auth.key_id).bind(auth.user_id).bind(auth.team_id).bind(held).bind(&snapshot).bind(simple).bind(db::now_s()).execute(&st.db).await?;
    }
    // An ambiguous failure keeps the durable hold. Never retry a create automatically.
    let response = send_json(
        &p.name,
        request(st, &p, &key, Method::POST, "/v1/batches")
            .await?
            .json(&v),
    )
    .await;
    let (status, value) = match response {
        Ok(pair) => pair,
        Err(e) => {
            sqlx::query("UPDATE batch_jobs SET status='uncertain' WHERE id=?")
                .bind(&intent)
                .execute(&st.db)
                .await?;
            return Err(e);
        }
    };
    if !status.is_success() {
        if status.is_client_error() {
            sqlx::query("DELETE FROM batch_jobs WHERE id=?")
                .bind(&intent)
                .execute(&st.db)
                .await?;
        } else {
            sqlx::query("UPDATE batch_jobs SET status='uncertain' WHERE id=?")
                .bind(&intent)
                .execute(&st.db)
                .await?;
        }
        return Ok((status, Json(value)).into_response());
    }
    let id = value["id"]
        .as_str()
        .filter(|s| valid_id(s))
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream returned an invalid batch ID.",
            )
        })?;
    let batch = Object {
        kind: "batch".into(),
        id: id.into(),
        key_id: auth.key_id,
        provider: o.provider.clone(),
        provider_key: o.provider_key,
        credential_hash: o.credential_hash.clone(),
        base_url: o.base_url.clone(),
        created_at: db::now_s(),
        status: value["status"].as_str().unwrap_or("validating").into(),
        object_json: value.to_string(),
        manifest_json: None,
    };
    // Commit the identity and Reservation together; neither can be replaced by a duplicate ID.
    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO proxy_objects(kind,id,key_id,provider,provider_key,credential_hash,base_url,created_at,status,object_json) VALUES('batch',?,?,?,?,?,?,?,?,?)")
        .bind(id).bind(auth.key_id).bind(&batch.provider).bind(batch.provider_key).bind(&batch.credential_hash).bind(&batch.base_url).bind(batch.created_at).bind(&batch.status).bind(&batch.object_json).execute(&mut *tx).await?;
    sqlx::query("UPDATE batch_jobs SET id=?,status=? WHERE id=?")
        .bind(id)
        .bind(&batch.status)
        .bind(&intent)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    observe(st, &batch, &value).await?;
    Ok((status, Json(value)).into_response())
}

fn terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "expired" | "cancelled")
}
async fn observe(st: &AppState, o: &Object, value: &Value) -> Result<(), ApiError> {
    update(st, o, value).await?;
    let status = value["status"]
        .as_str()
        .ok_or_else(|| error_response(StatusCode::BAD_GATEWAY, "Batch status is missing."))?;
    sqlx::query("UPDATE batch_jobs SET status=?,last_poll=? WHERE id=?")
        .bind(status)
        .bind(db::now_s())
        .bind(&o.id)
        .execute(&st.db)
        .await?;
    for field in ["output_file_id", "error_file_id"] {
        if let Some(id) = value[field].as_str() {
            if !valid_id(id) {
                return Err(error_response(
                    StatusCode::BAD_GATEWAY,
                    "Invalid batch output ID.",
                ));
            }
            let existing: Option<Object> =
                sqlx::query_as("SELECT * FROM proxy_objects WHERE kind='file' AND id=?")
                    .bind(id)
                    .fetch_optional(&st.db)
                    .await?;
            if let Some(existing) = existing {
                if existing.key_id != o.key_id
                    || existing.provider != o.provider
                    || existing.credential_hash != o.credential_hash
                {
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        "Output file identity conflict.",
                    ));
                }
            } else {
                let (p, key) = pinned(st, o, None)?;
                let (status, metadata) = send_json(
                    &p.name,
                    request(st, &p, &key, Method::GET, &format!("/v1/files/{id}")).await?,
                )
                .await?;
                if !status.is_success() || metadata["id"].as_str() != Some(id) {
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        "Batch output metadata is not available.",
                    ));
                }
                let file = Object {
                    kind: "file".into(),
                    id: id.into(),
                    status: "available".into(),
                    object_json: metadata.to_string(),
                    manifest_json: None,
                    ..o.clone()
                };
                store(st, &file).await?;
            }
        }
    }
    if terminal(status) {
        account(st, o, value).await?;
    }
    Ok(())
}

async fn account(st: &AppState, o: &Object, value: &Value) -> Result<(), ApiError> {
    let job:Option<Job>=sqlx::query_as("SELECT j.key_id,j.user_id,j.team_id,j.snapshot_json,j.simple,u.name AS user_name FROM batch_jobs j JOIN users u ON u.id=j.user_id WHERE j.id=? AND j.accounted=0").bind(&o.id).fetch_optional(&st.db).await?;
    let Some(job) = job else {
        return Ok(());
    };
    let snapshot: Snapshot = serde_json::from_str(&job.snapshot_json)?;
    let inputs: std::collections::HashMap<_, _> = snapshot
        .lines
        .iter()
        .map(|l| (l.custom_id.as_str(), l))
        .collect();
    let (p, key) = pinned(st, o, None)?;
    let mut results = std::collections::HashMap::new();
    let mut remaining = 64 * 1024 * 1024;
    for field in ["output_file_id", "error_file_id"] {
        if let Some(id) = value[field].as_str() {
            let r = request(
                st,
                &p,
                &key,
                Method::GET,
                &format!("/v1/files/{id}/content"),
            )
            .await?
            .send()
            .await
            .map_err(|e| crate::egress::request_error(&p.name, e))?;
            if !r.status().is_success() {
                return Err(error_response(
                    StatusCode::BAD_GATEWAY,
                    "Batch output is not available for accounting.",
                ));
            }
            let data = proxy::read_capped(r, remaining, &p.name).await?;
            remaining -= data.len();
            for line in data
                .split(|b| *b == b'\n')
                .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
            {
                if line.len() > JSON_LIMIT || results.len() >= 10_000 {
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        "Batch output exceeds accounting limits.",
                    ));
                }
                let v: Value = serde_json::from_slice(line)?;
                let id = v["custom_id"]
                    .as_str()
                    .filter(|id| inputs.contains_key(id))
                    .ok_or_else(|| {
                        error_response(
                            StatusCode::BAD_GATEWAY,
                            "Unknown custom_id in batch output.",
                        )
                    })?
                    .to_owned();
                if results.insert(id, v).is_some() {
                    return Err(error_response(
                        StatusCode::BAD_GATEWAY,
                        "Duplicate custom_id in batch output.",
                    ));
                }
            }
        }
    }
    let successful = results
        .values()
        .filter(|r| {
            r["response"]["status_code"]
                .as_u64()
                .is_some_and(|s| (200..300).contains(&s))
        })
        .count() as u64;
    if (value["status"] == "completed" && results.len() != inputs.len())
        || value["request_counts"]["completed"]
            .as_u64()
            .is_some_and(|n| n != successful)
        || (value["status"] != "completed"
            && results.len() != inputs.len()
            && value["request_counts"]["completed"].as_u64().is_none())
    {
        return Err(error_response(
            StatusCode::BAD_GATEWAY,
            "Completed batch output is missing or incomplete.",
        ));
    }
    let mut entries = Vec::with_capacity(snapshot.lines.len());
    for line in &snapshot.lines {
        let result = results.get(&line.custom_id);
        let status = result
            .and_then(|r| r["response"]["status_code"].as_u64())
            .unwrap_or(499)
            .clamp(100, 599) as u16;
        let endpoint = if line.endpoint.ends_with("responses") {
            provider::Endpoint::Responses
        } else if line.endpoint.ends_with("embeddings") {
            provider::Endpoint::Embeddings
        } else {
            provider::Endpoint::ChatCompletions
        };
        let usage = result
            .and_then(|r| ProviderKind::Openai.usage_from_json(endpoint, &r["response"]["body"]));
        if (200..300).contains(&status) && usage.is_none() {
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                "Successful batch output has no usage. Accounting will retry.",
            ));
        }
        let usage = usage.unwrap_or_default().bounded();
        let frozen = &snapshot.models[&line.model];
        let mut quote = frozen.quote.clone();
        if job.simple {
            quote.unpriced = false;
        }
        quote.cost_micros = if job.simple {
            0
        } else {
            frozen.cost.batch_amount(&usage)
        };
        quote.price_micros = if job.simple {
            0
        } else {
            frozen.price.batch_amount(&usage)
        };
        entries.push(db::NewEntry {
            ts_ms: db::now_ms(),
            user_id: job.user_id,
            key_id: job.key_id,
            team_id: job.team_id,
            provider: o.provider.clone(),
            subscription: false,
            endpoint: "batch".into(),
            requested_model: Some(line.model.clone()),
            model: Some(line.model.clone()),
            status,
            streamed: false,
            cache_hit: false,
            usage,
            reported_cost_micros: None,
            quote,
            estimated: false,
            latency_ms: 0,
            ttft_ms: None,
            client_aborted: false,
            provider_key: Some(o.provider_key),
            attempts: 1,
            session_affinity: None,
        });
    }
    let _guard = st.budget_check.lock().await;
    let mut tx = db::begin_write(&st.db).await?;
    if sqlx::query("UPDATE batch_jobs SET accounted=1,held_micros=0 WHERE id=? AND accounted=0")
        .bind(&o.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 0
    {
        return Ok(());
    }
    for (line, entry) in snapshot.lines.iter().zip(&entries) {
        let id = db::insert_entry_in(&mut tx, entry).await?;
        sqlx::query("INSERT INTO batch_entries(batch_id,custom_id,ledger_id) VALUES(?,?,?)")
            .bind(&o.id)
            .bind(&line.custom_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    for entry in &entries {
        crate::metrics::record(&job.user_name, entry);
    }
    if !job.simple {
        use crate::ratelimit::Scope;
        st.limiter.record_tokens(
            &[Scope::Key(job.key_id), Scope::User(job.user_id)],
            entries.iter().map(|e| e.usage.total()).sum(),
            db::now_ms(),
        );
    }
    Ok(())
}

/// Inspect at most 20 unaccounted batches. Failed accounting retains its hold.
pub async fn poll_once(st: &AppState) -> anyhow::Result<usize> {
    let objects:Vec<Object>=sqlx::query_as("SELECT o.* FROM proxy_objects o JOIN batch_jobs j ON j.id=o.id WHERE o.kind='batch' AND j.accounted=0 AND j.status NOT IN ('creating','uncertain') ORDER BY j.last_poll,j.created_at,j.id LIMIT 20")
        .fetch_all(&st.db).await?;
    let count = objects.len();
    for o in objects {
        sqlx::query("UPDATE batch_jobs SET last_poll=? WHERE id=?")
            .bind(db::now_s())
            .bind(&o.id)
            .execute(&st.db)
            .await?;
        let outcome = async {
            let (p, key) = pinned(st, &o, None)?;
            let (status, value) = send_json(
                &p.name,
                request(st, &p, &key, Method::GET, &format!("/v1/batches/{}", o.id)).await?,
            )
            .await?;
            if !status.is_success() {
                return Err(error_response(
                    StatusCode::BAD_GATEWAY,
                    "Batch poll failed.",
                ));
            }
            observe(st, &o, &value).await
        }
        .await;
        if outcome.is_err() {
            tracing::warn!(batch_id=%o.id,"Batch poll/accounting failed; it will retry.");
        }
    }
    Ok(count)
}

/// The scheduler waits for each tracked tick, so slow Providers cannot overlap ticks.
pub fn start_background(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let worker = st.clone();
            st.spawn_tracked(async move {
                if let Err(e) = poll_once(&worker).await {
                    tracing::error!(error=%e,"Batch polling failed");
                }
                let _ = tx.send(());
            });
            if rx.await.is_err() {
                break;
            }
        }
    })
}
