//! The Proxy: authenticate a Virtual Key, forward to the Provider, record a
//! Ledger Entry from the usage that the Provider reports.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use crate::catalog::Catalog;
use crate::db::{self, KeyAuth, NewEntry};
use crate::provider::{Endpoint, ProviderKind, is_stream, model_of};
use crate::ratelimit::Scope;
use crate::state::{AppState, Provider, Reservation};
use crate::translate::{self, Direction, StreamTranslator};
use crate::usage::Usage;

fn formatted_error(status: StatusCode, kind: &str, msg: &str, anthropic: bool) -> Response {
    let mut body = json!({"error": {"type": kind, "message": msg}});
    if anthropic {
        body["type"] = json!("error");
    }
    (status, axum::Json(body)).into_response()
}

/// The single compatibility seam for native and translated deployments.
fn compatible(kind: ProviderKind, endpoint: Endpoint) -> bool {
    endpoint.native_kind().is_none_or(|native| native == kind)
        || translation(kind, endpoint).is_some()
}

fn translation(kind: ProviderKind, endpoint: Endpoint) -> Option<Direction> {
    match (endpoint, kind) {
        (Endpoint::ChatCompletions, ProviderKind::Anthropic) => Some(Direction::ChatToMessages),
        (Endpoint::Messages, ProviderKind::Openai) => Some(Direction::MessagesToChat),
        _ => None,
    }
}

fn prepare_body(
    candidate: &crate::routing::Candidate,
    endpoint: Endpoint,
    body: Option<&Value>,
) -> Result<(Option<Value>, Endpoint, bool), translate::TranslateError> {
    let dir = translation(candidate.provider.kind, endpoint);
    let target = dir.map_or(endpoint, |d| Endpoint::from_path(d.upstream_path()));
    let mut prepared = match dir {
        Some(dir) => Some(translate::translate_request(dir, body.unwrap_or(&Value::Null))?),
        None => body.cloned(),
    };
    if candidate.rewrite && let Some(v) = prepared.as_mut() {
        v["model"] = json!(candidate.model);
    }
    let strip = prepared.as_mut()
        .is_some_and(|v| candidate.provider.kind.prepare_body(target, v));
    Ok((prepared, target, strip))
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

async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
    anthropic: bool,
) -> Result<KeyAuth, Box<Response>> {
    let Some(key) = bearer(headers) else {
        return Err(Box::new(formatted_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing API key",
            anthropic,
        )));
    };
    match db::lookup_key(&st.db, &key).await {
        Ok(Some(a)) if !a.revoked && !a.user_disabled => Ok(a),
        Ok(_) => Err(Box::new(formatted_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid API key",
            anthropic,
        ))),
        Err(e) => {
            tracing::error!(error = %e, "key lookup failed");
            Err(Box::new(formatted_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "internal error",
                anthropic,
            )))
        }
    }
}

pub async fn list_models(State(st): State<AppState>, headers: HeaderMap) -> Response {
    models_response(st, headers, None).await
}

pub async fn get_model(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    models_response(st, headers, Some(id)).await
}

async fn models_response(st: AppState, headers: HeaderMap, id: Option<String>) -> Response {
    let anthropic = headers.contains_key("anthropic-version");
    let auth = match authenticate(&st, &headers, anthropic).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let catalog = st.catalog();
    let table = st.router_table();
    let mut models = std::collections::BTreeMap::new();
    for model in &catalog.models {
        models.insert(model.name.clone(), model.provider.clone());
    }
    for route in &table.routes {
        models.insert(route.name.clone(), "router".to_owned());
    }
    let data: Vec<Value> = models.into_iter()
        .filter(|(name, _)| table.allowed(&catalog, &auth.allowed_models, Some(name)))
        .map(|(name, owner)| {
            if anthropic {
                json!({"type":"model", "id":name, "display_name":name, "created_at":"1970-01-01T00:00:00Z"})
            } else {
                json!({"id":name, "object":"model", "created":0, "owned_by":owner})
            }
        }).collect();
    if let Some(id) = id {
        return match data.into_iter().find(|m| m["id"].as_str() == Some(&id)) {
            Some(model) => axum::Json(model).into_response(),
            None => formatted_error(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "model not found",
                anthropic,
            ),
        };
    }
    if anthropic {
        axum::Json(json!({"first_id": data.first().map(|m| &m["id"]), "last_id": data.last().map(|m| &m["id"]), "data":data, "has_more":false})).into_response()
    } else {
        axum::Json(json!({"object":"list", "data":data})).into_response()
    }
}

/// Headers that are not copied between client and upstream.
fn skip_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "authorization"
            | "x-api-key"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-connection"
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
    upstream_model: Option<String>,
    streamed: bool,
    started: Instant,
    ts_ms: i64,
    provider_key: Option<i64>,
    attempts: i64,
    rate_scopes: Vec<Scope>,
    /// Released after the Ledger Entry is written, so the amount is always
    /// counted as either reserved or spent.
    reservation: Option<Reservation>,
}

struct Outcome {
    status: u16,
    model: Option<String>,
    usage: Usage,
    estimated: bool,
    ttft_ms: Option<i64>,
    client_aborted: bool,
}

impl Outcome {
    fn failed(status: u16) -> Outcome {
        Outcome {
            status,
            model: None,
            usage: Usage::default(),
            estimated: false,
            ttft_ms: None,
            client_aborted: false,
        }
    }
}

impl Ctx {
    fn finish(self, o: Outcome) {
        self.st
            .limiter
            .record_tokens(&self.rate_scopes, o.usage.total(), db::now_ms());
        let catalog = self.st.catalog();
        let model = o.model.or_else(|| self.upstream_model.clone());
        let mut quote = catalog.quote(model.as_deref(), self.auth.team_id, &o.usage);
        // Failed requests with no usage are errors, not Unpriced Entries.
        quote.unpriced = quote.unpriced && o.status < 400;
        let entry = NewEntry {
            ts_ms: self.ts_ms,
            user_id: self.auth.user_id,
            key_id: self.auth.key_id,
            team_id: self.auth.team_id,
            provider: self.provider.name.clone(),
            endpoint: self.endpoint.name().into(),
            requested_model: self.requested_model.clone(),
            model,
            status: o.status,
            streamed: self.streamed,
            usage: o.usage,
            quote,
            estimated: o.estimated,
            latency_ms: self.started.elapsed().as_millis() as i64,
            ttft_ms: o.ttft_ms,
            client_aborted: o.client_aborted,
            provider_key: self.provider_key,
            attempts: self.attempts,
        };
        crate::metrics::record(&self.auth.user_name, &entry);
        let db = self.st.db.clone();
        let reservation = self.reservation;
        let fut = async move {
            if let Err(e) = db::insert_entry(&db, &entry).await {
                tracing::error!(error = %e, "ledger write failed");
            }
            drop(reservation);
        };
        self.st.spawn_tracked(fut);
    }
}

/// The worst-case Price of a request, for a Budget reservation: the request
/// size as input tokens (about 4 bytes per token) plus the maximum output.
fn worst_case_price(
    catalog: &Catalog,
    team_id: Option<i64>,
    endpoint: Endpoint,
    model: Option<&str>,
    body: Option<&Value>,
    body_len: usize,
    default_output: i64,
) -> i64 {
    let max_out = body
        .and_then(translate::output_token_limit)
        .and_then(Value::as_i64)
        .unwrap_or(default_output)
        .max(0);
    let mut usage = Usage {
        input: (body_len as i64 + 3) / 4,
        ..Default::default()
    };
    match endpoint {
        Endpoint::Embeddings => {}
        Endpoint::Images => usage.image_output = max_out,
        _ => usage.output = max_out,
    }
    catalog.quote(model, team_id, &usage).price_micros
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
    let endpoint = Endpoint::from_path(uri.path());
    let anthropic = endpoint == Endpoint::Messages;
    let error = |status, kind, msg: &str| formatted_error(status, kind, msg, anthropic);

    let auth = match authenticate(&st, &headers, anthropic).await {
        Ok(a) => a,
        Err(r) => return *r,
    };

    let json_body: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };
    let requested_model = json_body.as_ref().and_then(model_of);
    if endpoint == Endpoint::ChatCompletions
        && let Some(body) = &json_body
        && let Err(e) = translate::validate_chat_limits(body)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request_error", &e.message);
    }
    let catalog = st.catalog();
    let table = st.router_table();
    let settings = st.settings();
    let simple = settings.mode == "simple";

    if !table.allowed(&catalog, &auth.allowed_models, requested_model.as_deref()) {
        return error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this API key may not use this model",
        );
    }

    let scopes: Vec<_> = [
        (Scope::Key(auth.key_id), auth.key_limits),
        (Scope::User(auth.user_id), auth.user_limits),
    ]
    .into_iter()
    .filter(|(_, limits)| !simple && (limits.rpm.is_some() || limits.tpm.is_some()))
    .collect();
    if !simple && let Err(limited) = st.limiter.admit(&scopes, db::now_ms()) {
        let scope = match limited.scope {
            Scope::Key(_) => "key",
            Scope::User(_) => "user",
        };
        metrics::counter!("llm_rate_limited_total", "scope" => scope).increment(1);
        let mut response = error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &format!("{scope} {} per minute limit exceeded", limited.which),
        );
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&limited.retry_after_secs.to_string()).expect("integer header"),
        );
        return response;
    }

    let anthropic_hint = headers.contains_key("anthropic-version");
    let mut candidates = st.candidates(
        &table,
        &catalog,
        requested_model.as_deref(),
        endpoint,
        anthropic_hint,
    );
    candidates.retain(|c| compatible(c.provider.kind, endpoint));
    let Some(first) = candidates.first() else {
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "no provider is configured for this request",
        );
    };

    let mut reservation = None;
    if !simple && let Some(budget) = auth.budget_micros {
        let spent = match db::spend_since(
            &st.db,
            auth.user_id,
            db::budget_period_start(auth.budget_monthly),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "budget check failed");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "internal error",
                );
            }
        };
        let worst = candidates
            .iter()
            .filter_map(|c| {
                let (prepared, target, _) = prepare_body(c, endpoint, json_body.as_ref()).ok()?;
                let len = prepared.as_ref().map_or(body.len(), |v| v.to_string().len());
                Some(worst_case_price(
                    &catalog,
                    auth.team_id,
                    target,
                    c.model.as_deref(),
                    prepared.as_ref(),
                    len,
                    settings.reserve_output_tokens,
                ))
            })
            .max()
            .unwrap_or(0);
        match st.reserve(auth.user_id, budget, spent, worst) {
            Some(r) => reservation = Some(r),
            None => {
                return error(
                    StatusCode::PAYMENT_REQUIRED,
                    "budget_exceeded",
                    "the budget for this user does not cover this request",
                );
            }
        }
    }

    let streamed = json_body.as_ref().is_some_and(is_stream);
    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let mut ctx = Ctx {
        st: st.clone(),
        auth,
        provider: first.provider.clone(),
        endpoint,
        requested_model,
        upstream_model: first.model.clone(),
        streamed,
        started,
        ts_ms,
        provider_key: None,
        attempts: 0,
        rate_scopes: scopes.into_iter().map(|(scope, _)| scope).collect(),
        reservation,
    };

    // Try each deployment's keys before the next deployment. Nothing has
    // been sent to the client, so connection errors, 429 and 5xx can retry.
    let degraded = candidates
        .iter()
        .all(|c| !st.deployment_available(c, &table.settings));
    let mut upstream = None;
    let mut strip_usage_chunk = false;
    let mut direction = None;
    let mut upstream_endpoint = endpoint;
    let mut previous_route = None;
    'deployments: for candidate in &candidates {
        if ctx.attempts as usize >= table.settings.max_attempts {
            break;
        }
        let Some(permit) = st.acquire_deployment(candidate, &table.settings, degraded) else {
            continue;
        };
        let provider = &candidate.provider;
        let dir = translation(provider.kind, endpoint);
        let (prepared, target_endpoint, strip) = match prepare_body(candidate, endpoint, json_body.as_ref()) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, "invalid_request_error", &e.message),
        };
        let upstream_body = match prepared {
            Some(v) => Bytes::from(serde_json::to_vec(&v).unwrap_or_default()),
            None => body.clone(),
        };
        let path = dir.map_or_else(
            || path_q.to_owned(),
            |d| {
                let mut path = d.upstream_path().to_owned();
                if let Some(query) = uri.query() {
                    path.push('?');
                    path.push_str(query);
                }
                path
            },
        );
        let url = format!("{}{}", provider.base_url.trim_end_matches('/'), path);
        let order: Vec<Option<usize>> = match provider.key_order() {
            o if o.is_empty() => vec![None],
            o => o.into_iter().map(Some).collect(),
        };
        for idx in order {
            if ctx.attempts as usize >= table.settings.max_attempts {
                break 'deployments;
            }
            if ctx.attempts > 0 {
                metrics::counter!("llm_upstream_retries_total", "provider" => ctx.provider.name.clone()).increment(1);
            }
            if let Some(from) = previous_route.take() {
                metrics::counter!("llm_upstream_fallbacks_total", "from" => from, "to" => candidate.route.clone()).increment(1);
            }
            ctx.provider = provider.clone();
            ctx.upstream_model = candidate.model.clone();
            ctx.attempts += 1;
            ctx.provider_key = idx.map(|i| i as i64);
            strip_usage_chunk = strip;
            direction = dir;
            upstream_endpoint = target_endpoint;
            let key = idx.map(|i| provider.keys[i].as_str()).unwrap_or("");
            let req = build_request(&st, provider, key, &method, &url, &headers, anthropic_hint);
            upstream = None;
            match req.body(upstream_body.clone()).send().await {
                Ok(r) => {
                    let healthy = !retryable(r.status());
                    permit.record(healthy);
                    upstream = Some(r);
                    if healthy {
                        break 'deployments;
                    }
                }
                Err(e) => {
                    permit.record(false);
                    tracing::warn!(error = %e, url = %url, "upstream request failed");
                }
            }
            if permit.is_probe() {
                break;
            }
        }
        previous_route = Some(candidate.route.clone());
    }
    let Some(upstream) = upstream else {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        );
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
        let translator = direction.map(|dir| {
            out_headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            StreamTranslator::new(dir, ctx.upstream_model.as_deref().unwrap_or_default())
                .with_usage(
                    json_body
                        .as_ref()
                        .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                )
        });
        return stream_response(
            ctx,
            upstream,
            status,
            out_headers,
            strip_usage_chunk && direction.is_none(),
            upstream_endpoint,
            translator,
        );
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "upstream body read failed");
            ctx.finish(Outcome::failed(502));
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream response failed",
            );
        }
    };
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    let kind = ctx.provider.kind;
    let usage = parsed
        .as_ref()
        .and_then(|v| kind.usage_from_json(upstream_endpoint, v));
    let model = parsed.as_ref().and_then(model_of);
    let estimated = usage.is_none() && status.is_success() && endpoint.is_accounted();
    ctx.finish(Outcome {
        status: status.as_u16(),
        model,
        usage: usage.unwrap_or_default(),
        estimated,
        ttft_ms: None,
        client_aborted: false,
    });

    let bytes = if let Some(dir) = direction {
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if status.is_success() {
            Bytes::from(
                serde_json::to_vec(&translate::translate_response(
                    dir,
                    parsed.as_ref().unwrap_or(&Value::Null),
                ))
                .unwrap_or_default(),
            )
        } else {
            Bytes::from(translate::translate_error(dir, status.as_u16(), &bytes))
        }
    } else {
        bytes
    };
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

fn retryable(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error()
}

fn build_request(
    st: &AppState,
    provider: &Provider,
    key: &str,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    anthropic_hint: bool,
) -> reqwest::RequestBuilder {
    let mut req = st.http().request(method.clone(), url);
    for (name, value) in headers.iter() {
        if !skip_header(name)
            && !(provider.kind == ProviderKind::Openai && name.as_str().starts_with("anthropic-"))
        {
            req = req.header(name, value);
        }
    }
    match provider.kind {
        ProviderKind::Openai => req.bearer_auth(key),
        ProviderKind::Anthropic => {
            let r = req.header("x-api-key", key);
            if anthropic_hint {
                r
            } else {
                r.header("anthropic-version", "2023-06-01")
            }
        }
    }
}

/// Forward an SSE stream. If the client disconnects, keep reading the
/// upstream stream to get the final usage (ADR 0001).
fn stream_response(
    ctx: Ctx,
    upstream: reqwest::Response,
    status: StatusCode,
    headers: HeaderMap,
    strip_usage_chunk: bool,
    upstream_endpoint: Endpoint,
    mut translator: Option<StreamTranslator>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let mut tracker = ctx
        .provider
        .kind
        .new_stream_tracker(upstream_endpoint, strip_usage_chunk);
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
                    let out = tracker.feed(&chunk);
                    let out = if let Some(t) = translator.as_mut() {
                        t.feed(&out)
                    } else {
                        out
                    };
                    if ttft.is_none() && tracker.saw_content {
                        ttft = Some(started.elapsed().as_millis() as i64);
                    }
                    if !client_gone
                        && !out.is_empty()
                        && tx.send(Ok(Bytes::from(out))).await.is_err()
                    {
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
        let rest = tracker.finish();
        let rest = if let Some(t) = translator.as_mut() {
            let mut out = t.feed(&rest);
            out.extend(t.finish());
            out
        } else {
            rest
        };
        if !client_gone && !rest.is_empty() {
            let _ = tx.send(Ok(Bytes::from(rest))).await;
        }
        drop(tx);
        let estimated = status.is_success() && !tracker.usage_final;
        let code = if upstream_failed && !tracker.usage_final {
            502
        } else {
            status.as_u16()
        };
        ctx.finish(Outcome {
            status: code,
            model: tracker.model.take(),
            usage: tracker.usage.unwrap_or_default(),
            estimated,
            ttft_ms: ttft,
            client_aborted: client_gone,
        });
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
