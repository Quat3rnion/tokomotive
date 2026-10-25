//! The Proxy: authenticate a Virtual Key, forward to the Provider, record a
//! Ledger Entry from the usage that the Provider reports.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use crate::db::{self, KeyAuth, NewEntry};
use crate::provider::{Endpoint, ProviderKind, is_stream, model_of};
use crate::state::{AppState, Provider};
use crate::usage::Usage;

fn error(status: StatusCode, kind: &str, msg: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"type": kind, "message": msg}})),
    )
        .into_response()
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(k) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
    {
        return Some(k.trim().to_owned());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned())
}

/// Headers that are not copied between client and upstream.
fn skip_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "authorization" | "x-api-key" | "content-length" | "connection" | "keep-alive"
            | "transfer-encoding" | "te" | "upgrade" | "proxy-authorization" | "proxy-connection"
            // We parse the response, so ask for an uncompressed body.
            | "accept-encoding"
    )
}

/// Request context that is needed to write the Ledger Entry.
struct Ctx {
    st: AppState,
    auth: KeyAuth,
    provider: Provider,
    endpoint: Endpoint,
    requested_model: Option<String>,
    streamed: bool,
    started: Instant,
    ts_ms: i64,
}

impl Ctx {
    fn finish(
        self,
        status: u16,
        model: Option<String>,
        usage: Usage,
        estimated: bool,
        ttft_ms: Option<i64>,
        client_aborted: bool,
    ) {
        let catalog = self.st.catalog();
        let model = model.or_else(|| self.requested_model.clone());
        let mut quote = catalog.quote(model.as_deref(), self.auth.team_id, &usage);
        // Failed requests with no usage are errors, not Unpriced Entries.
        quote.unpriced = quote.unpriced && status < 400;
        let entry = NewEntry {
            ts_ms: self.ts_ms,
            user_id: self.auth.user_id,
            key_id: self.auth.key_id,
            team_id: self.auth.team_id,
            provider: self.provider.name.clone(),
            endpoint: endpoint_name(self.endpoint).into(),
            requested_model: self.requested_model.clone(),
            model,
            status,
            streamed: self.streamed,
            usage,
            quote,
            estimated,
            latency_ms: self.started.elapsed().as_millis() as i64,
            ttft_ms,
            client_aborted,
        };
        crate::metrics::record(&self.auth.user_name, &entry);
        let db = self.st.db.clone();
        let fut = async move {
            if let Err(e) = db::insert_entry(&db, &entry).await {
                tracing::error!(error = %e, "ledger write failed");
            }
        };
        self.st.spawn_tracked(fut);
    }
}

fn endpoint_name(e: Endpoint) -> &'static str {
    match e {
        Endpoint::ChatCompletions => "chat_completions",
        Endpoint::Responses => "responses",
        Endpoint::Messages => "messages",
        Endpoint::Other => "other",
    }
}

pub async fn handle(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let ts_ms = db::now_ms();

    let Some(key) = bearer(&headers) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing API key",
        );
    };
    let auth = match db::lookup_key(&st.db, &key).await {
        Ok(Some(a)) if !a.revoked && !a.user_disabled => a,
        Ok(_) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid API key",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "key lookup failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "internal error",
            );
        }
    };

    let endpoint = Endpoint::from_path(uri.path());
    let mut json_body: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };
    let requested_model = json_body.as_ref().and_then(model_of);
    let catalog = st.catalog();

    if !catalog.allowed(&auth.allowed_models, requested_model.as_deref()) {
        return error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this API key may not use this model",
        );
    }

    if let Some(budget) = auth.budget_micros {
        match db::spend_since(
            &st.db,
            auth.user_id,
            db::budget_period_start(auth.budget_monthly),
        )
        .await
        {
            Ok(spent) if spent >= budget => {
                return error(
                    StatusCode::PAYMENT_REQUIRED,
                    "budget_exceeded",
                    "the budget for this user is used up",
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "budget check failed");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "internal error",
                );
            }
        }
    }

    let anthropic_hint = headers.contains_key("anthropic-version");
    let Some(provider) = st.pick_provider(
        &catalog,
        requested_model.as_deref(),
        endpoint,
        anthropic_hint,
    ) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "no provider is configured for this request",
        );
    };

    let streamed = json_body.as_ref().is_some_and(is_stream);
    let body = match json_body.as_mut() {
        Some(v) if endpoint.is_accounted() => {
            provider.kind.prepare_body(endpoint, v);
            Bytes::from(serde_json::to_vec(v).unwrap_or_default())
        }
        _ => body,
    };

    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{}", provider.base_url.trim_end_matches('/'), path_q);
    let mut req = st.http.request(method, &url);
    for (name, value) in headers.iter() {
        if !skip_header(name) {
            req = req.header(name, value);
        }
    }
    req = match provider.kind {
        ProviderKind::Openai => req.bearer_auth(&provider.key),
        ProviderKind::Anthropic => {
            let r = req.header("x-api-key", &provider.key);
            if anthropic_hint {
                r
            } else {
                r.header("anthropic-version", "2023-06-01")
            }
        }
    };

    let ctx = Ctx {
        st: st.clone(),
        auth,
        provider,
        endpoint,
        requested_model,
        streamed,
        started,
        ts_ms,
    };

    let upstream = match req.body(body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "upstream request failed");
            ctx.finish(502, None, Usage::default(), false, None, false);
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream request failed",
            );
        }
    };

    let status = upstream.status();
    let mut out_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if !skip_header(name) && name != header::CONTENT_ENCODING {
            out_headers.append(name.clone(), value.clone());
        }
    }
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));

    if is_sse {
        return stream_response(ctx, upstream, status, out_headers);
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "upstream body read failed");
            ctx.finish(502, None, Usage::default(), false, None, false);
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream response failed",
            );
        }
    };
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    let kind = ctx.provider.kind;
    let usage = parsed.as_ref().and_then(|v| kind.usage_from_json(v));
    let model = parsed.as_ref().and_then(model_of);
    let estimated = usage.is_none() && status.is_success() && endpoint.is_accounted();
    ctx.finish(
        status.as_u16(),
        model,
        usage.unwrap_or_default(),
        estimated,
        None,
        false,
    );

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

/// Forward an SSE stream. If the client disconnects, keep reading the
/// upstream stream to get the final usage (ADR 0001).
fn stream_response(
    ctx: Ctx,
    upstream: reqwest::Response,
    status: StatusCode,
    headers: HeaderMap,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let mut tracker = ctx.provider.kind.new_stream_tracker();
    let started = ctx.started;
    let st = ctx.st.clone();
    let task = async move {
        let mut stream = upstream.bytes_stream();
        let mut ttft = None;
        let mut client_gone = false;
        let mut upstream_failed = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    tracker.feed(&chunk);
                    if ttft.is_none() && tracker.saw_content {
                        ttft = Some(started.elapsed().as_millis() as i64);
                    }
                    if !client_gone && tx.send(Ok(chunk)).await.is_err() {
                        client_gone = true;
                        tracing::debug!("client disconnected; draining upstream stream");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "upstream stream failed");
                    upstream_failed = true;
                    if !client_gone {
                        let _ = tx.send(Err(std::io::Error::other(e))).await;
                    }
                    break;
                }
            }
        }
        drop(tx);
        tracker.finish();
        let estimated = status.is_success() && !tracker.usage_final;
        let code = if upstream_failed && !tracker.usage_final {
            502
        } else {
            status.as_u16()
        };
        ctx.finish(
            code,
            tracker.model.take(),
            tracker.usage.unwrap_or_default(),
            estimated,
            ttft,
            client_gone,
        );
    };
    // Not tied to the client connection: the task continues after a disconnect.
    st.spawn_tracked(task);

    let mut resp = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}
