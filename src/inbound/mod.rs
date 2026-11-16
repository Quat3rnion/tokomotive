//! Native client adapters. All requests enter the accounted proxy before send.
mod bedrock;
mod gemini;
mod stream;

use crate::{
    provider::{Endpoint, ProviderKind},
    state::AppState,
    translate::TranslateError,
};
use axum::{
    body::{Body, Bytes},
    extract::{FromRequest, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Gemini,
    Converse,
    Invoke,
}
#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub format: Format,
    pub operation: String,
    pub original: Value,
    pub sse: bool,
}
fn invalid(message: &str) -> TranslateError {
    TranslateError {
        message: message.into(),
    }
}
fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(crate) fn safe_upstream_header(name: &axum::http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-type" | "retry-after" | "x-request-id" | "x-amzn-requestid"
    )
}
fn resource(value: &str) -> Result<String, TranslateError> {
    let value = value
        .replace('+', "%2B")
        .replace('&', "%26")
        .replace('=', "%3D");
    serde_urlencoded::from_str::<Vec<(String, String)>>(&format!("model={value}"))
        .ok()
        .and_then(|p| p.into_iter().next().map(|(_, v)| v))
        .ok_or_else(|| invalid("Invalid model resource"))
}

impl Context {
    pub fn retain_response(&self, kind: ProviderKind, raw: &Value, canonical: &[u8]) -> Bytes {
        // This field is internal cache metadata. Never trust an upstream copy.
        let mut value: Value = serde_json::from_slice(canonical).unwrap_or_default();
        if let Some(object) = value.as_object_mut() {
            object.remove("_tokomotive_native");
        }
        if !matches!(
            (self.format, kind),
            (Format::Gemini, ProviderKind::Gemini) | (Format::Converse, ProviderKind::Bedrock)
        ) {
            return Bytes::from(value.to_string());
        }
        // Only non-content metadata may cross the canonical guardrail boundary.
        // Never retain candidates, thought parts or Provider extension objects.
        let mut metadata = json!({});
        if self.format == Format::Gemini {
            for key in ["modelVersion", "responseId"] {
                if let Some(v) = raw.get(key).filter(|v| v.is_string()) {
                    metadata[key] = v.clone();
                }
            }
        } else if raw["metrics"]["latencyMs"].is_u64() {
            metadata["metrics"] = json!({"latencyMs": raw["metrics"]["latencyMs"]});
        }
        // Optional metadata must not reduce the usable response body limit.
        if value.is_object()
            && canonical
                .len()
                .saturating_add(metadata.to_string().len())
                .saturating_add(32)
                <= 64 * 1024 * 1024
        {
            value["_tokomotive_native"] = metadata;
        }
        Bytes::from(value.to_string())
    }
    pub fn cache_identity(&self) -> Value {
        json!([format!("{:?}", self.format), self.operation, self.original])
    }
    pub fn semantic_identity(&self) -> Value {
        let mut options = self.original.clone();
        let field = match self.format {
            Format::Gemini => "contents",
            Format::Converse => "messages",
            // Provider-specific Invoke bodies have no common normalization.
            Format::Invoke => return self.cache_identity(),
        };
        if let Some(obj) = options.as_object_mut() {
            obj.remove(field);
        }
        if let Some(obj) = options
            .get_mut("generateContentRequest")
            .and_then(Value::as_object_mut)
        {
            obj.remove(field);
        }
        json!([format!("{:?}", self.format), self.operation, options])
    }
    pub fn is_gemini_aux(&self) -> bool {
        self.format == Format::Gemini
            && !matches!(
                self.operation.as_str(),
                "generateContent" | "streamGenerateContent"
            )
    }
    pub fn prepare(
        &self,
        kind: ProviderKind,
        body: &Value,
    ) -> Result<Option<Value>, TranslateError> {
        let same = matches!(
            (self.format, kind),
            (Format::Gemini, ProviderKind::Gemini) | (Format::Converse, ProviderKind::Bedrock)
        );
        if !same {
            if self.format == Format::Gemini
                && body.get("response_format").is_some()
                && kind != ProviderKind::Openai
            {
                return Err(invalid(
                    "responseMimeType/responseSchema cannot be translated to this Provider format",
                ));
            }
            return Ok(None);
        }
        if self.is_gemini_aux() {
            return Ok(Some(gemini::aux_request(&self.operation, body)?));
        }
        // Always rebuild from the guarded canonical request. Native fields are
        // retained only by the explicit allowlist below, never by raw restore.
        let mut out = if self.format == Format::Gemini {
            crate::translate_gemini::request(Endpoint::ChatCompletions, body)?
        } else {
            crate::translate_bedrock::request(Endpoint::ChatCompletions, body)?
        };
        if self.format == Format::Gemini
            && let Some(safety) = self.original.get("safetySettings")
        {
            out["safetySettings"] = safety.clone();
        }
        Ok(Some(out))
    }
    pub fn upstream_path(&self, model: &str, stream: bool) -> Result<String, TranslateError> {
        let path = crate::translate_gemini::path(model, stream)?;
        if !self.is_gemini_aux() {
            return Ok(path);
        }
        Ok(format!(
            "{}:{}",
            path.split(':').next().unwrap_or_default(),
            self.operation
        ))
    }
    pub fn auxiliary_response(&self, v: &Value) -> Option<Value> {
        self.is_gemini_aux().then(|| if self.operation == "countTokens" { json!({"input_tokens":v["totalTokens"]}) } else {
            let embeddings = if self.operation == "embedContent" { vec![v["embedding"].clone()] } else { array(&v["embeddings"]).to_vec() };
            json!({"object":"list","data":embeddings.iter().enumerate().map(|(i,e)|json!({"object":"embedding","index":i,"embedding":e["values"]})).collect::<Vec<_>>(),"usage":{"prompt_tokens":0,"total_tokens":0}})
        })
    }
}

pub(crate) fn rewrite_embedding_models(mut value: Value, model: &str) -> Value {
    if let Some(requests) = value["requests"].as_array_mut() {
        for r in requests {
            r["model"] = json!(format!("models/{model}"));
        }
    } else {
        value["model"] = json!(format!("models/{model}"));
    }
    value
}

fn credentials(uri: &Uri, headers: &mut HeaderMap) {
    let query: Vec<(String, String)> =
        serde_urlencoded::from_str(uri.query().unwrap_or_default()).unwrap_or_default();
    let key = headers.remove("x-goog-api-key").or_else(|| {
        query
            .into_iter()
            .find(|(k, _)| k == "key")
            .and_then(|(_, v)| HeaderValue::from_str(&v).ok())
    });
    if !headers.contains_key(header::AUTHORIZATION)
        && let Some(key) = key
    {
        headers.insert("x-api-key", key);
    }
}

pub async fn handle(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    mut headers: HeaderMap,
    request: Request,
) -> Response {
    let path = uri.path();
    let format = if path.starts_with("/model/") {
        if path.ends_with("/invoke") || path.ends_with("/invoke-with-response-stream") {
            Format::Invoke
        } else {
            Format::Converse
        }
    } else {
        Format::Gemini
    };
    credentials(&uri, &mut headers);
    if format != Format::Gemini
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("AWS4-"))
    {
        return error(
            format,
            StatusCode::UNAUTHORIZED,
            "Use a Virtual Key as Bearer. SigV4 signatures are not verified.",
        );
    }
    if let Err(response) = crate::proxy::authenticate(&st, &headers, false).await {
        return error(format, response.status(), "Invalid or missing Virtual Key.");
    }
    if method != Method::POST {
        return error(
            format,
            StatusCode::METHOD_NOT_ALLOWED,
            "Use POST for this operation.",
        );
    }
    // Authenticate before buffering. The Bytes extractor still enforces the
    // router's DefaultBodyLimit, including for requests without Content-Length.
    let body = match Bytes::from_request(request, &st).await {
        Ok(b) => b,
        Err(e) => {
            return error(
                format,
                e.status(),
                "The request body exceeds the limit or cannot be read.",
            );
        }
    };
    let original: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error(
                format,
                StatusCode::BAD_REQUEST,
                "The request must contain valid JSON.",
            );
        }
    };
    let parsed = if format == Format::Gemini {
        gemini::request(&uri, &original)
    } else {
        bedrock::request(&uri, &original, format)
    };
    let (canonical, operation, target) = match parsed {
        Ok(v) => v,
        Err(e) => return error(format, StatusCode::BAD_REQUEST, &e.message),
    };
    let context = Context {
        format,
        operation,
        original,
        sse: uri
            .query()
            .is_some_and(|q| q.split('&').any(|p| p == "alt=sse")),
    };
    // The client URL and its key never enter traces, cache URLs or the Ledger.
    let response = crate::proxy::handle_native(
        st,
        method,
        target.parse().expect("static path"),
        headers,
        Bytes::from(canonical.to_string()),
        context.clone(),
    )
    .await;
    finish(context, response).await
}

fn gemini_discovery(uri: &Uri, headers: &HeaderMap) -> bool {
    if uri.path().starts_with("/v1beta/")
        || headers.contains_key("x-goog-api-key")
        || serde_urlencoded::from_str::<Vec<(String, String)>>(uri.query().unwrap_or_default())
            .unwrap_or_default()
            .iter()
            .any(|(k, _)| k == "key")
    {
        return true;
    }
    if headers.contains_key("anthropic-version") {
        return false;
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("google-genai"))
}

pub async fn list_models(
    State(st): State<AppState>,
    uri: Uri,
    mut headers: HeaderMap,
    Query(query): Query<crate::proxy::ModelsQuery>,
) -> Response {
    if !gemini_discovery(&uri, &headers) {
        return crate::proxy::list_models(State(st), headers, Query(query)).await;
    }
    credentials(&uri, &mut headers);
    headers.remove("anthropic-version");
    let response = crate::proxy::list_models(State(st), headers, Query(query)).await;
    models(response, true).await
}
pub async fn get_model(
    State(st): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    mut headers: HeaderMap,
) -> Response {
    if !gemini_discovery(&uri, &headers) {
        return crate::proxy::get_model(State(st), Path(id), headers).await;
    }
    credentials(&uri, &mut headers);
    headers.remove("anthropic-version");
    let response = crate::proxy::get_model(State(st), Path(id), headers).await;
    models(response, false).await
}
async fn models(response: Response, list: bool) -> Response {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    if !parts.status.is_success() {
        return error(Format::Gemini, parts.status, message(&v));
    }
    let model = |v: &Value| json!({"name":format!("models/{}",v["id"].as_str().unwrap_or_default()),"displayName":v["id"],"supportedGenerationMethods":["generateContent","countTokens","embedContent","batchEmbedContents"]});
    axum::Json(if list {
        json!({"models":array(&v["data"]).iter().map(model).collect::<Vec<_>>()})
    } else {
        model(&v)
    })
    .into_response()
}
fn message(v: &Value) -> &str {
    v["error"]["message"]
        .as_str()
        .or_else(|| v["message"].as_str())
        .unwrap_or("The request failed.")
}
pub(crate) fn error(format: Format, status: StatusCode, message: &str) -> Response {
    if format == Format::Gemini {
        let name = match status.as_u16() {
            400 | 413 => "INVALID_ARGUMENT",
            401 => "UNAUTHENTICATED",
            402 | 403 => "PERMISSION_DENIED",
            404 => "NOT_FOUND",
            429 => "RESOURCE_EXHAUSTED",
            503 => "UNAVAILABLE",
            _ => "INTERNAL",
        };
        (
            status,
            axum::Json(json!({"error":{"code":status.as_u16(),"message":message,"status":name}})),
        )
            .into_response()
    } else {
        let name = match status.as_u16() {
            400 | 413 => "ValidationException",
            401..=403 => "AccessDeniedException",
            404 => "ResourceNotFoundException",
            429 => "ThrottlingException",
            _ => "ServiceUnavailableException",
        };
        (
            status,
            [("x-amzn-errortype", name)],
            axum::Json(json!({"message":message})),
        )
            .into_response()
    }
}
async fn finish(context: Context, mut response: Response) -> Response {
    if context.original.get("safetySettings").is_some() {
        response.headers_mut().insert(
            "x-tokomotive-safety-settings",
            HeaderValue::from_static("applied-only-to-gemini-upstreams"),
        );
    }
    if response.status().is_success()
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        return stream::response(context, response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    if !parts.status.is_success() {
        let mut response = error(context.format, parts.status, message(&value));
        for (key, value) in &parts.headers {
            if (safe_upstream_header(key) && key != header::CONTENT_TYPE)
                || matches!(
                    key.as_str(),
                    "x-tokomotive-cache" | "x-tokomotive-safety-settings"
                )
            {
                response.headers_mut().insert(key, value.clone());
            }
        }
        return response;
    }
    let retained = value
        .as_object_mut()
        .and_then(|o| o.remove("_tokomotive_native"));
    // The inspected canonical form is always the source of response content,
    // including on cache hits. Raw native content is never restored.
    let mut value = if context.format == Format::Gemini {
        gemini::response(&context.operation, &value)
    } else if context.format == Format::Converse {
        bedrock::response(&value)
    } else {
        value
    };
    if let Some(metadata) = retained {
        if context.format == Format::Gemini {
            for key in ["modelVersion", "responseId"] {
                if let Some(v) = metadata.get(key).filter(|v| v.is_string()) {
                    value[key] = v.clone();
                }
            }
        } else if context.format == Format::Converse && metadata["metrics"]["latencyMs"].is_u64() {
            value["metrics"] = json!({"latencyMs": metadata["metrics"]["latencyMs"]});
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(value.to_string()))
}
