//! The Proxy: authenticate a Virtual Key, forward to the Provider, record a
//! Ledger Entry from the usage that the Provider reports.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
#[cfg(test)]
#[path = "proxy_bedrock_tests.rs"]
mod bedrock_tests;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use crate::catalog::Catalog;
use crate::db::{self, KeyAuth, NewEntry};
use crate::provider::{Endpoint, ProviderKind, is_stream, model_of};
use crate::ratelimit::Scope;
use crate::state::{AppState, Provider, Reservation};
use crate::translate::{self, Direction, StreamTranslator};
use crate::translate_responses::{self, ProxyStream, ResponseStream};
use crate::usage::Usage;

#[path = "proxy_extra.rs"]
mod extra;

fn formatted_error(status: StatusCode, kind: &str, msg: &str, anthropic: bool) -> Response {
    let mut body = json!({"error": {"type": kind, "message": msg}});
    if anthropic {
        body["type"] = json!("error");
    }
    (status, axum::Json(body)).into_response()
}

fn model_not_found(message: &str, anthropic: bool) -> Response {
    let body = if anthropic {
        json!({"type":"error", "error":{"type":"not_found_error", "message":message}})
    } else {
        json!({"error":{"type":"invalid_request_error", "code":"model_not_found", "message":message}})
    };
    (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

/// The largest non-streaming upstream body that the Proxy reads. The same
/// as the request body limit; base64 images can make responses large.
const MAX_UPSTREAM_BODY: usize = 64 * 1024 * 1024;

/// Read a response body, and stop with an error when it passes `max` bytes.
pub(crate) async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
    provider: &str,
) -> anyhow::Result<Bytes> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        anyhow::bail!("upstream body is larger than {max} bytes");
    }
    let mut out = bytes::BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        if e.is_timeout() {
            anyhow::Error::new(crate::provider_limits::RequestTimeout)
        } else {
            crate::egress::request_error(provider, e)
        }
    })? {
        if out.len() + chunk.len() > max {
            anyhow::bail!("upstream body is larger than {max} bytes");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out.freeze())
}

/// The single compatibility seam for native and translated deployments.
fn compatible(kind: ProviderKind, endpoint: Endpoint) -> bool {
    if endpoint == Endpoint::Rerank {
        return matches!(kind, ProviderKind::Cohere | ProviderKind::Openai);
    }
    if endpoint == Endpoint::CountTokens {
        return compatible(kind, Endpoint::Messages);
    }
    if matches!(
        kind,
        ProviderKind::Gemini | ProviderKind::Bedrock | ProviderKind::Cohere
    ) {
        return matches!(endpoint, Endpoint::ChatCompletions | Endpoint::Messages);
    }
    endpoint.native_kind().is_none_or(|native| native == kind)
        || translation(kind, endpoint).is_some()
}

pub(crate) fn provider_allowed(auth: &KeyAuth, provider: &Provider) -> bool {
    if auth.allowed_providers.is_empty() {
        provider.auth == crate::provider::ProviderAuth::ApiKey
    } else {
        auth.allowed_providers.contains(&provider.name)
    }
}

/// True when a path has a "." or ".." segment, also percent-encoded, or a
/// backslash. URL parsers resolve these, so the upstream path could leave
/// the path that the client asked for.
fn has_dot_segment(path: &str) -> bool {
    path.contains('\\')
        || path.split('/').any(|seg| {
            let seg = seg.to_ascii_lowercase().replace("%2e", ".");
            seg == "." || seg == ".."
        })
}

/// A subscription credential is for model requests only. Only the canonical
/// path of an accounted endpoint may carry it.
fn subscription_path_allowed(endpoint: Endpoint, path_q: &str) -> bool {
    let path = path_q.split('?').next().unwrap_or("").trim_end_matches('/');
    match endpoint {
        Endpoint::ChatCompletions => path == "/v1/chat/completions",
        Endpoint::Responses => path == "/v1/responses",
        Endpoint::Messages => path == "/v1/messages",
        Endpoint::CountTokens => path == "/v1/messages/count_tokens",
        Endpoint::Rerank => path == "/v1/rerank",
        Endpoint::AudioTranscription => path == "/v1/audio/transcriptions",
        Endpoint::AudioTranslation => path == "/v1/audio/translations",
        Endpoint::AudioSpeech => path == "/v1/audio/speech",
        Endpoint::Embeddings => path == "/v1/embeddings",
        Endpoint::Images => path == "/v1/images/generations" || path == "/v1/images/edits",
        Endpoint::Other => false,
    }
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
    if matches!(endpoint, Endpoint::CountTokens | Endpoint::Rerank) {
        let mut prepared = body.cloned().unwrap_or(Value::Null);
        if candidate.rewrite {
            prepared["model"] = json!(candidate.model);
        }
        if candidate.provider.auth == crate::provider::ProviderAuth::Claude {
            crate::oauth::prepare_claude(&mut prepared);
        }
        return Ok((Some(prepared), endpoint, false));
    }
    if candidate.provider.kind == ProviderKind::Cohere {
        let mut prepared =
            crate::translate_cohere::request(endpoint, body.unwrap_or(&Value::Null))?;
        if let Some(model) = &candidate.model {
            prepared["model"] = json!(model);
        }
        return Ok((Some(prepared), Endpoint::Other, false));
    }
    if candidate.provider.kind == ProviderKind::Bedrock {
        return Ok((
            Some(crate::translate_bedrock::request(
                endpoint,
                body.unwrap_or(&Value::Null),
            )?),
            Endpoint::Other,
            false,
        ));
    }
    if candidate.provider.kind == ProviderKind::Gemini {
        return Ok((
            Some(crate::translate_gemini::request(
                endpoint,
                body.unwrap_or(&Value::Null),
            )?),
            Endpoint::Other,
            false,
        ));
    }
    let codex = candidate.provider.auth == crate::provider::ProviderAuth::Codex;
    if codex
        && !matches!(
            endpoint,
            Endpoint::ChatCompletions | Endpoint::Responses | Endpoint::Messages
        )
    {
        return Err(translate::TranslateError { message:"Codex accepts only model requests on /v1/responses, /v1/chat/completions or /v1/messages".into() });
    }
    let dir = translation(candidate.provider.kind, endpoint);
    let mut target = dir.map_or(endpoint, |d| Endpoint::from_path(d.upstream_path()));
    let mut prepared = match dir {
        Some(dir) => Some(translate::translate_request(
            dir,
            body.unwrap_or(&Value::Null),
        )?),
        None => body.cloned(),
    };
    // Messages has no stream_options. These options were added by Translation,
    // not by the client, so the Provider's switch must also control them.
    if dir == Some(Direction::MessagesToChat)
        && !candidate.provider.include_usage
        && let Some(obj) = prepared.as_mut().and_then(Value::as_object_mut)
    {
        obj.remove("stream_options");
    }
    if candidate.provider.auth == crate::provider::ProviderAuth::Claude
        && target == Endpoint::Messages
        && prepared.as_ref().is_none_or(|body| !body.is_object())
    {
        return Err(translate::TranslateError {
            message: "Messages requests must be JSON objects".into(),
        });
    }
    if candidate.rewrite
        && let Some(v) = prepared.as_mut()
    {
        v["model"] = json!(candidate.model);
    }
    if codex {
        let mut value = prepared.unwrap_or(Value::Null);
        if target == Endpoint::ChatCompletions {
            value = translate_responses::request(&value)?;
        }
        translate_responses::prepare_codex(&mut value)?;
        prepared = Some(value);
        target = Endpoint::Responses;
    }
    let strip = prepared.as_mut().is_some_and(|v| {
        candidate
            .provider
            .kind
            .prepare_body(target, v, candidate.provider.include_usage)
    });
    if candidate.provider.auth == crate::provider::ProviderAuth::Claude
        && let Some(body) = prepared.as_mut()
    {
        crate::oauth::prepare_claude(body);
    }
    if candidate.provider.auth == crate::provider::ProviderAuth::GoogleVertex {
        let body = prepared
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or_else(|| translate::TranslateError {
                message: "Vertex Messages requests must be JSON objects".into(),
            })?;
        body.remove("model");
        body.insert("anthropic_version".into(), json!("vertex-2023-10-16"));
    }
    Ok((prepared, target, strip))
}

fn prepare_inbound_body(
    candidate: &crate::routing::Candidate,
    endpoint: Endpoint,
    body: Option<&Value>,
    native: Option<&crate::inbound::Context>,
) -> Result<(Option<Value>, Endpoint, bool), translate::TranslateError> {
    if let (Some(native), Some(body)) = (native, body) {
        let mut body = body.clone();
        if let Some(prepared) = native.prepare(candidate.provider.kind, &body)? {
            return Ok((
                Some(if endpoint == Endpoint::Embeddings {
                    // Batch embedding requests carry a model resource in each item.
                    let model = candidate.model.as_deref().unwrap_or_default();
                    crate::inbound::rewrite_embedding_models(prepared, model)
                } else {
                    prepared
                }),
                if native.is_gemini_aux() {
                    endpoint
                } else {
                    Endpoint::Other
                },
                false,
            ));
        }
        if endpoint == Endpoint::CountTokens
            && native.format == crate::inbound::Format::Gemini
            && candidate.provider.kind == ProviderKind::Anthropic
        {
            body = translate::translate_request(Direction::ChatToMessages, &body)?;
            if let Some(obj) = body.as_object_mut() {
                obj.remove("max_tokens");
                obj.remove("stream");
            }
        }
        return prepare_body(candidate, endpoint, Some(&body));
    }
    prepare_body(candidate, endpoint, body)
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

pub(crate) async fn authenticate(
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
        Ok(Some(a))
            if !a.revoked && !a.user_disabled && a.expires_at.is_some_and(|t| t <= db::now_s()) =>
        {
            Err(Box::new(formatted_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Virtual Key expired",
                anthropic,
            )))
        }
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

#[derive(Default, serde::Deserialize)]
pub struct ModelsQuery {
    limit: Option<i64>,
    after_id: Option<String>,
    before_id: Option<String>,
}

pub async fn list_models(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ModelsQuery>,
) -> Response {
    models_response(st, headers, None, q).await
}

pub async fn get_model(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    models_response(st, headers, Some(id), ModelsQuery::default()).await
}

async fn models_response(
    st: AppState,
    headers: HeaderMap,
    id: Option<String>,
    q: ModelsQuery,
) -> Response {
    let anthropic = headers.contains_key("anthropic-version");
    let auth = match authenticate(&st, &headers, anthropic).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let catalog = st.catalog();
    let table = st.router_table();
    let access = crate::resolve::Access {
        models: &auth.allowed_models,
        providers: &auth.allowed_providers,
    };
    if let Some(id) = id {
        let index = st.resolution_index.read().expect("resolution index");
        let r = crate::resolve::resolve(&table, &catalog, &index, &id, access);
        if r.allowed(access, &id, &catalog)
            && let Some(owner) = r.owner(&table, &catalog, access)
        {
            return axum::Json(model_description(&r.canonical, &owner, anthropic)).into_response();
        }
        return model_not_found(
            &crate::resolve::message(&table, &catalog, &index, &id, access, &r),
            anthropic,
        );
    }
    if anthropic && q.after_id.is_some() && q.before_id.is_some() {
        return formatted_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Use after_id or before_id, not both.",
            true,
        );
    }
    let limit = if anthropic {
        q.limit.unwrap_or(50).clamp(1, 200) as usize
    } else {
        1000
    };
    let reverse = anthropic && q.before_id.is_some();
    let mut models = std::collections::BTreeMap::new();
    let mut count = 0;
    // Routing snapshots are already resident. Keep only one bounded response window.
    for (name, owner) in crate::resolve::names(
        &table,
        &catalog,
        &st.resolution_index.read().expect("resolution index"),
        access,
    ) {
        if anthropic && q.after_id.as_ref().is_some_and(|after| &name <= after)
            || anthropic && q.before_id.as_ref().is_some_and(|before| &name >= before)
        {
            continue;
        }
        count += 1;
        models.insert(name, owner);
        if models.len() > limit + 1 {
            if reverse {
                models.pop_first();
            } else {
                models.pop_last();
            }
        }
    }
    if !anthropic && count > 1000 {
        tracing::warn!(count, cap = 1000, "OpenAI Model list exceeds server cap");
    }
    let has_more = models.len() > limit;
    if has_more {
        if reverse {
            models.pop_first();
        } else {
            models.pop_last();
        }
    }
    let data: Vec<Value> = models
        .into_iter()
        .map(|(name, owner)| model_description(&name, &owner, anthropic))
        .collect();
    if anthropic {
        axum::Json(json!({"first_id": data.first().map(|m| &m["id"]), "last_id": data.last().map(|m| &m["id"]), "data":data, "has_more":has_more})).into_response()
    } else {
        axum::Json(json!({"object":"list", "data":data})).into_response()
    }
}

fn model_description(name: &str, owner: &str, anthropic: bool) -> Value {
    if anthropic {
        json!({"type":"model", "id":name, "display_name":name, "created_at":"1970-01-01T00:00:00Z"})
    } else {
        json!({"id":name, "object":"model", "created":0, "owned_by":owner})
    }
}

/// Headers that are not copied between client and upstream.
fn skip_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "authorization"
            | "x-api-key"
            | "api-key"
            | "x-goog-api-key"
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
    provider_permit: Option<crate::provider_limits::Permit>,
    route: String,
    guardrails: crate::guardrails::Session,
    cache_plan: Option<crate::cache::Plan>,
    semantic_plan: Option<crate::semantic_cache::WritePlan>,
    semantic_cache_hit: bool,
    semantic_cache_embed: bool,
    cache_hit_price: Option<i64>,
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
    trace: crate::observability::RequestTrace,
    session_affinity: Option<&'static str>,
    rate_scopes: Vec<Scope>,
    /// Released after the Ledger Entry is written, so the amount is always
    /// counted as either reserved or spent.
    reservation: Option<Reservation>,
}

struct Outcome {
    status: u16,
    model: Option<String>,
    usage: Usage,
    reported_cost_micros: Option<i64>,
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
            reported_cost_micros: None,
            estimated: false,
            ttft_ms: None,
            client_aborted: false,
        }
    }
}

impl Ctx {
    fn resolution_headers(&self, headers: &mut HeaderMap) {
        for (key, value) in [
            ("x-tokomotive-route", self.route.as_str()),
            ("x-tokomotive-provider", self.provider.name.as_str()),
            (
                "x-tokomotive-model",
                self.upstream_model.as_deref().unwrap_or_default(),
            ),
        ] {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(key, value);
            }
        }
    }
    fn finish(mut self, mut o: Outcome) {
        self.trace.finish(o.status, o.ttft_ms, self.ts_ms);
        if self.endpoint == Endpoint::CountTokens {
            o.usage = Usage::default();
        }
        // Upstream counts are untrusted: clamp them before any sum or price.
        o.usage = o.usage.bounded();
        if !o.client_aborted
            && self.endpoint != Endpoint::CountTokens
            && self.cache_hit_price.is_none()
        {
            self.trace
                .adaptive_feedback(self.streamed, o.usage.output, &self.st.settings().router);
        }
        if self.cache_hit_price.is_none() {
            self.st
                .limiter
                .record_tokens(&self.rate_scopes, o.usage.total(), db::now_ms());
        }
        let catalog = self.st.catalog();
        let mut model = o.model.or_else(|| self.upstream_model.clone());
        if self.provider.auth == crate::provider::ProviderAuth::GoogleVertex
            && let Some(name) = model.as_deref()
            && let Ok((_, id)) = crate::vertex::model(name)
        {
            model = Some(id.into());
        }
        let mut quote = catalog.quote(model.as_deref(), self.auth.team_id, &o.usage);
        if self.endpoint == Endpoint::CountTokens {
            // Counts are metadata, not inference. Keep a zero-cost audit entry.
            quote = Default::default();
        }
        if self.provider.auth.is_subscription() {
            quote.cost_micros = 0;
            quote.cost_card_id = None;
        }
        if let Some(price) = self.cache_hit_price {
            quote.cost_micros = 0;
            quote.price_micros = price;
            quote.cost_card_id = None;
            quote.price_card_id = None;
            quote.unpriced = false;
        }
        // Failed requests with no usage are errors, not Unpriced Entries.
        quote.unpriced = quote.unpriced && o.status < 400;
        let entry = NewEntry {
            semantic_cache_hit: self.semantic_cache_hit,
            semantic_cache_embed: self.semantic_cache_embed,
            route_rule: self.trace.route_rule.clone(),
            cache_hit: self.cache_hit_price.is_some(),
            ts_ms: self.ts_ms,
            user_id: self.auth.user_id,
            key_id: self.auth.key_id,
            team_id: self.auth.team_id,
            provider: self.provider.name.clone(),
            subscription: self.provider.auth.is_subscription(),
            endpoint: self.endpoint.name().into(),
            requested_model: self.requested_model.clone(),
            model,
            status: o.status,
            streamed: self.streamed,
            usage: o.usage,
            reported_cost_micros: o.reported_cost_micros,
            quote,
            estimated: o.estimated,
            latency_ms: self.started.elapsed().as_millis() as i64,
            ttft_ms: o.ttft_ms,
            client_aborted: o.client_aborted,
            provider_key: self.provider_key,
            attempts: self.attempts,
            session_affinity: self.session_affinity.map(str::to_owned),
        };
        crate::metrics::record(&self.auth.user_name, &entry);
        self.st.observability.export(
            &entry,
            &self.trace,
            &self.auth.user_name,
            &self.auth.key_name,
        );
        let db = self.st.db.clone();
        let alerts_st = self.st.clone();
        let alerts_auth = self.auth;
        let reservation = self.reservation;
        let fut = async move {
            let mut entry = entry;
            if self.guardrails.wait().await {
                entry.status = 400;
            }
            if let Some(status) = self.guardrails.hooks.status() {
                entry.status = status;
            }
            let pending = crate::ledger_spool::PendingEntry::new(entry.clone())
                .with_attempts(&self.trace.attempts);
            let written = pending.persist_reserved(&alerts_st, reservation).await;
            match written {
                None => {}
                Some(id) => {
                    self.guardrails.hooks.ledger(id, &entry).await;
                    if let Err(e) = self.guardrails.ledger(&db, id).await {
                        tracing::error!(error=%e,"guardrail ledger write failed");
                    }
                    if let Err(e) = self
                        .trace
                        .store(&db, &alerts_st.secret_key, id, entry.ts_ms)
                        .await
                    {
                        tracing::error!(error=%e,"request diagnostics write failed");
                    }
                    if let Err(e) =
                        crate::alerts::check_budgets(&alerts_st, &alerts_auth, entry.ts_ms / 1000)
                            .await
                    {
                        tracing::warn!(error=%e,"budget alert check failed");
                    }
                }
            }
        };
        self.st.spawn_tracked(fut);
    }

    fn cache_write(
        &self,
        usage: Usage,
        model: Option<String>,
        estimated: bool,
    ) -> Option<crate::cache::Write> {
        if self.guardrails.hooks.has_post() {
            return None;
        }
        if self.cache_plan.is_none() && self.semantic_plan.is_none() {
            return None;
        }
        Some(crate::cache::Write {
            st: self.st.clone(),
            plan: self.cache_plan.clone(),
            semantic: self.semantic_plan.clone(),
            entry: crate::cache::Entry {
                body: String::new(),
                content_type: String::new(),
                provider: self.provider.name.clone(),
                upstream_model: self.upstream_model.clone(),
                model: model.or_else(|| self.upstream_model.clone()),
                usage,
                estimated,
            },
        })
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
    if endpoint == Endpoint::CountTokens {
        return 0;
    }
    if endpoint == Endpoint::Rerank {
        // A provider can split long documents into search units. Reserve one
        // unit per request byte, as well as one token per byte for token APIs.
        return catalog
            .quote(
                model,
                team_id,
                &Usage {
                    service_units: body_len as i64,
                    input: body_len as i64,
                    ..Default::default()
                },
            )
            .price_micros;
    }
    if extra::audio_upload(endpoint) || endpoint == Endpoint::AudioSpeech {
        let units = if endpoint == Endpoint::AudioSpeech {
            body.and_then(|v| v["input"].as_str())
                .map_or(0, |s| s.chars().count())
        } else {
            body_len
        };
        return catalog
            .quote(
                model,
                team_id,
                &Usage {
                    service_units: units as i64,
                    ..Default::default()
                },
            )
            .price_micros;
    }
    let max_out = body
        .and_then(|body| {
            translate::output_token_limit(body)
                .or_else(|| body.get("generationConfig")?.get("maxOutputTokens"))
                .or_else(|| body.get("inferenceConfig")?.get("maxTokens"))
        })
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
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let guardrails = crate::guardrails::Session::default();
    let mut completion = guardrails.completion();
    let endpoint = Endpoint::from_path(uri.path());
    let anthropic = matches!(endpoint, Endpoint::Messages | Endpoint::CountTokens);
    let responses = endpoint == Endpoint::Responses;
    let response = handle_inner(st, method, uri, headers, body, guardrails.clone()).await;
    let response = guardrails.response(response, anthropic, responses).await;
    completion.disarm();
    response
}

pub(crate) async fn handle_native(
    st: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    native: crate::inbound::Context,
) -> Response {
    let guardrails = crate::guardrails::Session::default();
    let mut completion = guardrails.completion();
    let anthropic = matches!(
        Endpoint::from_path(uri.path()),
        Endpoint::Messages | Endpoint::CountTokens
    );
    let response = handle_inner_native(
        st,
        method,
        uri,
        headers,
        Ok(body),
        guardrails.clone(),
        Some(native),
        false,
    )
    .await;
    let response = guardrails.response(response, anthropic, false).await;
    completion.disarm();
    response
}

async fn handle_inner(
    st: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
    guardrails: crate::guardrails::Session,
) -> Response {
    handle_inner_native(st, method, uri, headers, body, guardrails, None, false).await
}

// Keep internal identity out of client-controlled headers. Run the request to
// completion even when the cache wait expires, so usage is still accounted.
fn semantic_embedding(
    st: AppState,
    headers: &HeaderMap,
    plan: &crate::semantic_cache::Plan,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<crate::semantic_cache::Embedding>> + Send>,
> {
    let mut auth_headers = HeaderMap::new();
    auth_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    for name in ["authorization", "x-api-key", "api-key"] {
        if let Some(value) = headers.get(name) {
            auth_headers.insert(name, value.clone());
        }
    }
    let body = Bytes::from(
        json!({"model":plan.policy.embedding_model,"input":plan.text,"encoding_format":"float"})
            .to_string(),
    );
    let timeout = std::time::Duration::from_millis(plan.policy.timeout_ms);
    Box::pin(async move {
        let (send, receive) = tokio::sync::oneshot::channel();
        let state = st.clone();
        st.spawn_tracked(async move {
            let table = state.router_table();
            let guardrails = crate::guardrails::Session::default();
            let mut completion = guardrails.completion();
            let response = handle_inner_native(
                state.clone(),
                Method::POST,
                Uri::from_static("/v1/embeddings"),
                auth_headers,
                Ok(body),
                guardrails.clone(),
                None,
                true,
            )
            .await;
            let response = guardrails.response(response, false, false).await;
            completion.disarm();
            // Never tag an in-flight vector with a replacement configuration.
            // The internal request can resolve before or after a settings save.
            let vector = if response.status().is_success()
                && std::sync::Arc::ptr_eq(&table, &state.router_table())
            {
                let provider = response
                    .headers()
                    .get("x-tokomotive-provider")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let model = response
                    .headers()
                    .get("x-tokomotive-model")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let source = table
                    .providers
                    .iter()
                    .find(|p| p.name == provider)
                    .map(|p| json!([p.base_url, p.kind, p.auth.as_str()]));
                axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                    .await
                    .ok()
                    .and_then(|b| {
                        let version = serde_json::from_slice::<Value>(&b)
                            .ok()?
                            .get("model")
                            .cloned();
                        Some(crate::semantic_cache::Embedding {
                            vector: crate::semantic_cache::parse_embedding(&b)?,
                            identity: json!([provider, model, source, version]).to_string(),
                        })
                    })
            } else {
                None
            };
            let _ = send.send(vector);
        });
        tokio::time::timeout(timeout, receive)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_inner_native(
    st: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, axum::extract::rejection::BytesRejection>,
    guardrails: crate::guardrails::Session,
    native: Option<crate::inbound::Context>,
    internal_embedding: bool,
) -> Response {
    let started = Instant::now();
    let ts_ms = db::now_ms();
    let endpoint = Endpoint::from_path(uri.path());
    let anthropic = matches!(endpoint, Endpoint::Messages | Endpoint::CountTokens);
    let error = |status, kind, msg: &str| formatted_error(status, kind, msg, anthropic);

    let auth = match authenticate(&st, &headers, anthropic).await {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let mut body = match body {
        Ok(body) => body,
        Err(rejection) => {
            return error(
                rejection.status(),
                "invalid_request_error",
                "request body exceeds the limit or cannot be read",
            );
        }
    };
    if crate::batches::reserved_path(uri.path()) {
        return error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "Object not found.",
        );
    }
    if has_dot_segment(uri.path()) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request path is not valid",
        );
    }

    let audio_upload = if extra::audio_upload(endpoint) {
        if body.len() > st.settings().audio_upload_limit_mib.clamp(1, 63) * 1024 * 1024 {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "audio upload exceeds the configured size limit",
            );
        }
        match extra::AudioUpload::parse(&headers, body.clone()).await {
            Ok(upload) => Some(upload),
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "invalid multipart audio upload; model and one file are required",
                );
            }
        }
    } else {
        None
    };
    let mut json_body: Option<Value> = if let Some(upload) = &audio_upload {
        Some(json!({"model":upload.model}))
    } else if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };
    let requested_model = json_body.as_ref().and_then(model_of);
    if endpoint == Endpoint::AudioSpeech {
        if body.len() > extra::COUNT_BODY_LIMIT {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "speech body exceeds 1 MiB",
            );
        }
        if requested_model.is_none()
            || json_body
                .as_ref()
                .is_none_or(|v| v["input"].as_str().is_none_or(str::is_empty))
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model and non-empty input are required",
            );
        }
    }
    if endpoint == Endpoint::Rerank {
        if body.len() > extra::COUNT_BODY_LIMIT {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "rerank body exceeds 1 MiB",
            );
        }
        if requested_model.is_none()
            || json_body.as_ref().is_none_or(|v| {
                !v["query"].is_string()
                    || v["documents"].as_array().is_none_or(Vec::is_empty)
                    || is_stream(v)
            })
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model, query and non-empty documents are required; streaming is not supported",
            );
        }
    }
    if endpoint == Endpoint::CountTokens {
        if body.len() > extra::COUNT_BODY_LIMIT {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "token count body exceeds 1 MiB",
            );
        }
        if requested_model.is_none() || json_body.as_ref().is_none_or(|v| !v["messages"].is_array())
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model and messages are required",
            );
        }
    }
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

    let access = crate::resolve::Access {
        models: &auth.allowed_models,
        providers: &auth.allowed_providers,
    };
    let name = requested_model.as_deref().unwrap_or_default();
    let resolution = {
        let index = st.resolution_index.read().expect("resolution index");
        let resolution = crate::resolve::resolve(&table, &catalog, &index, name, access);
        if resolution.target.is_none() {
            return model_not_found(
                &crate::resolve::message(&table, &catalog, &index, name, access, &resolution),
                endpoint == Endpoint::Messages
                    || endpoint == Endpoint::CountTokens
                    || headers.contains_key("anthropic-version"),
            );
        }
        resolution
    };
    if !resolution.allowed(access, name, &catalog) {
        return error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this API key may not use this model",
        );
    }
    let resolved_route = match &resolution.target {
        Some(crate::resolve::Target::Route(name)) => Some(name.as_str()),
        _ => None,
    };

    let scopes: Vec<_> = [
        (Scope::Key(auth.key_id), auth.key_limits),
        (Scope::User(auth.user_id), auth.user_limits),
    ]
    .into_iter()
    .filter(|(_, limits)| !simple && (limits.rpm.is_some() || limits.tpm.is_some()))
    .collect();
    if !simple
        && !guardrails.hooks.pre_done()
        && let Err(limited) = st.limiter.admit(&scopes, db::now_ms())
    {
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

    if !guardrails.hooks.pre_done() {
        let metadata = ["x-tokomotive-meta-project", "x-tokomotive-meta-environment"]
            .into_iter()
            .filter_map(|name| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 1024)
                    .map(|v| (name.to_owned(), Value::String(v.to_owned())))
            })
            .collect::<serde_json::Map<_, _>>();
        if guardrails.hooks.configure(&st, resolved_route, auth.team_id, auth.key_id,
            json!({"endpoint":endpoint.name(),"route":resolved_route,"model":requested_model,"key_id":auth.key_id,"team_id":auth.team_id,"user":auth.user_name,"metadata":metadata,"body":json_body})
        ).await.is_err() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "Hooks unavailable.");
        }
    }
    let anthropic_hint = headers.contains_key("anthropic-version");
    let mut matched_rule = None;
    let rule_table = if let Some(route) = resolved_route {
        match crate::route_rules::load(&st.db, route).await {
            Ok(rules) => {
                let attributes = crate::route_rules::Attributes {
                    key: auth.key_id,
                    user: auth.user_id,
                    team: auth.team_id,
                    headers: &headers,
                    body: json_body.as_ref(),
                    model: name,
                    endpoint: endpoint.name(),
                    format: match native.as_ref().map(|n| n.format) {
                        Some(crate::inbound::Format::Gemini) => "gemini",
                        Some(_) => "bedrock",
                        None if anthropic => "anthropic",
                        None => "openai",
                    },
                };
                if let Some(selection) = crate::route_rules::select(&rules, &attributes) {
                    matched_rule = Some(selection.matched());
                    metrics::counter!("tokomotive_route_rule_matches_total","route"=>route.to_owned(),"rule"=>selection.rule.id.clone(),"side"=>selection.side).increment(1);
                    if let crate::route_rules::Action::Reject { message } = selection.action {
                        let entry = NewEntry {
                            semantic_cache_hit: false,
                            semantic_cache_embed: internal_embedding,
                            route_rule: matched_rule.clone(),
                            cache_hit: false,
                            ts_ms,
                            user_id: auth.user_id,
                            key_id: auth.key_id,
                            team_id: auth.team_id,
                            provider: String::new(),
                            subscription: false,
                            endpoint: endpoint.name().into(),
                            requested_model: requested_model.clone(),
                            model: None,
                            status: 403,
                            streamed: json_body.as_ref().is_some_and(is_stream),
                            usage: Usage::default(),
                            reported_cost_micros: None,
                            quote: Default::default(),
                            estimated: false,
                            latency_ms: started.elapsed().as_millis() as i64,
                            ttft_ms: None,
                            client_aborted: false,
                            provider_key: None,
                            attempts: 0,
                            session_affinity: None,
                        };
                        crate::metrics::record(&auth.user_name, &entry);
                        let mut trace = crate::observability::RequestTrace::new(
                            &st,
                            Some(route),
                            auth.key_id,
                            &[],
                            table.settings.max_attempts,
                        )
                        .await;
                        trace.route_rule = matched_rule.clone();
                        st.observability
                            .export(&entry, &trace, &auth.user_name, &auth.key_name);
                        let state = st.clone();
                        let hooks = guardrails.hooks.clone();
                        st.spawn_tracked(async move {
                            let pending = crate::ledger_spool::PendingEntry::new(entry.clone());
                            if let Some(id) = pending.persist_reserved(&state, None).await {
                                hooks.ledger(id, &entry).await;
                            }
                        });
                        return error(StatusCode::FORBIDDEN, "permission_error", message);
                    }
                    Some(crate::route_rules::apply(
                        &table,
                        route,
                        selection.action,
                        selection.rule.no_fallback,
                    ))
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::error!(error=%e,"Route rules unavailable");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "Route rules unavailable",
                );
            }
        }
    } else {
        None
    };
    let table = rule_table
        .map(std::sync::Arc::new)
        .unwrap_or_else(|| table.clone());
    let mut candidates = st.resolved_candidates(&table, &catalog, &resolution);
    for c in &mut candidates {
        if c.provider.auth == crate::provider::ProviderAuth::GoogleVertex
            && let Ok((kind, _)) = crate::vertex::model(c.model.as_deref().unwrap_or_default())
        {
            c.provider.kind = kind;
        }
    }
    candidates.retain(|c| {
        compatible(c.provider.kind, endpoint)
            || (endpoint == Endpoint::Embeddings
                && c.provider.kind == ProviderKind::Gemini
                && c.provider.auth == crate::provider::ProviderAuth::ApiKey
                && native.is_some())
    });
    let denied = candidates
        .iter()
        .find(|c| !provider_allowed(&auth, &c.provider))
        .map(|c| c.provider.name.clone());
    candidates.retain(|c| provider_allowed(&auth, &c.provider));
    let quota_schedule = match crate::quota::Schedule::load(&st.db).await {
        Ok(schedule) => schedule,
        Err(e) => {
            tracing::error!(error=%e,"account scheduling lookup failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "account scheduling unavailable",
            );
        }
    };
    quota_schedule.order_deployments(
        &table,
        &mut candidates,
        chrono::Utc::now().timestamp(),
        settings.router.quota_exhausted_percent,
    );
    st.adaptive.order(
        &table,
        &mut candidates,
        &settings.router,
        endpoint == Endpoint::AudioSpeech || json_body.as_ref().is_some_and(is_stream),
        |c| {
            st.deployment_available(c, &table.settings)
                && if c.provider.keys.is_empty() {
                    quota_schedule
                        .skip(
                            &c.provider,
                            None,
                            chrono::Utc::now().timestamp(),
                            settings.router.quota_exhausted_percent,
                            table.settings.cooldown_secs,
                        )
                        .is_none()
                } else {
                    (0..c.provider.keys.len()).any(|i| {
                        quota_schedule
                            .skip(
                                &c.provider,
                                Some(i),
                                chrono::Utc::now().timestamp(),
                                settings.router.quota_exhausted_percent,
                                table.settings.cooldown_secs,
                            )
                            .is_none()
                    })
                }
        },
    );
    // An older pin must not promote a Fallback ahead of a rule's chosen set.
    let affinity_count = if matched_rule.is_some() {
        candidates
            .iter()
            .take_while(|c| Some(c.route.as_str()) == resolved_route)
            .count()
    } else {
        candidates.len()
    };
    let mut session = st.session_order(
        &table,
        resolved_route,
        auth.user_id,
        &headers,
        json_body.as_ref(),
        &mut candidates[..affinity_count],
    );
    if candidates.is_empty()
        && let Some(name) = denied
    {
        return error(
            StatusCode::FORBIDDEN,
            "permission_error",
            &format!("this API key may not use provider {name}"),
        );
    }
    let Some(first) = candidates.first() else {
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "no provider is configured for this request",
        );
    };

    // Request rules run before cache identity/lookup and before body capture.
    match crate::guardrails::load(&st.db, resolved_route, auth.team_id, auth.key_id).await {
        Ok(engine) => guardrails.configure(engine),
        Err(e) => {
            tracing::error!(error=%e,"guardrail load failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Guardrail configuration could not be loaded.",
            );
        }
    }
    if let Some(value) = &mut json_body {
        if let Some(name) = guardrails.request(value).await {
            let entry = NewEntry {
                semantic_cache_hit: false,
                semantic_cache_embed: internal_embedding,
                route_rule: matched_rule.clone(),
                cache_hit: false,
                ts_ms,
                user_id: auth.user_id,
                key_id: auth.key_id,
                team_id: auth.team_id,
                provider: first.provider.name.clone(),
                subscription: false,
                endpoint: endpoint.name().into(),
                requested_model: requested_model.clone(),
                model: first.model.clone(),
                status: 400,
                streamed: value
                    .get("stream")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                usage: Usage::default(),
                reported_cost_micros: None,
                quote: Default::default(),
                estimated: false,
                latency_ms: started.elapsed().as_millis() as i64,
                ttft_ms: None,
                client_aborted: false,
                provider_key: None,
                attempts: 0,
                session_affinity: None,
            };
            crate::metrics::record(&auth.user_name, &entry);
            let state = st.clone();
            let session = guardrails.clone();
            st.spawn_tracked(async move {
                let pending = crate::ledger_spool::PendingEntry::new(entry.clone());
                if let Some(id) = pending.persist_reserved(&state, None).await {
                    session.hooks.ledger(id, &entry).await;
                    if let Err(e) = session.ledger(&state.db, id).await {
                        tracing::error!(error=%e,"guardrail ledger write failed");
                    }
                }
            });
            return crate::guardrails::blocked(&name, anthropic);
        }
        if audio_upload.is_none() {
            body = Bytes::from(value.to_string());
        }
    }

    if !guardrails.hooks.pre_done() {
        let hook_admission = if !simple && guardrails.hooks.has_pre() {
            let mut worst = 0;
            for c in &candidates {
                let Ok((prepared, target, _)) =
                    prepare_inbound_body(c, endpoint, json_body.as_ref(), native.as_ref())
                else {
                    continue;
                };
                let pricing_model =
                    if c.provider.auth == crate::provider::ProviderAuth::GoogleVertex {
                        c.model
                            .as_deref()
                            .and_then(|n| crate::vertex::model(n).ok())
                            .map(|(_, id)| id)
                    } else {
                        c.model.as_deref()
                    };
                let model_id = pricing_model.and_then(|m| catalog.resolve(m)).map(|m| m.id);
                let output = match crate::models_dev::reservation_output(
                    &st.db,
                    model_id,
                    settings.reserve_output_tokens,
                )
                .await
                {
                    Ok(n) => n,
                    Err(_) => {
                        return error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "api_error",
                            "Budget admission unavailable.",
                        );
                    }
                };
                worst = worst.max(worst_case_price(
                    &catalog,
                    auth.team_id,
                    target,
                    pricing_model,
                    prepared.as_ref(),
                    if extra::audio_upload(endpoint) {
                        body.len()
                    } else {
                        prepared
                            .as_ref()
                            .map_or(body.len(), |v| v.to_string().len())
                    },
                    output,
                ));
            }
            match crate::budgets::reserve(&st, &auth, worst, ts_ms / 1000).await {
                Ok(Some(r)) => Some(r),
                Ok(None) => {
                    return error(
                        StatusCode::PAYMENT_REQUIRED,
                        "budget_exceeded",
                        "Budget does not cover this request.",
                    );
                }
                Err(_) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "api_error",
                        "Budget admission unavailable.",
                    );
                }
            }
        } else {
            None
        };
        if let Some(value) = &mut json_body {
            let result = guardrails
                .hooks
                .pre(value, |value| {
                    if audio_upload.is_some() {
                        return Err("Hooks cannot modify multipart uploads.");
                    }
                    if !value.is_object()
                        || model_of(value).is_none()
                        || value.to_string().len() > 64 * 1024 * 1024
                    {
                        return Err("Hook returned an invalid request body.");
                    }
                    if endpoint == Endpoint::ChatCompletions
                        && translate::validate_chat_limits(value).is_err()
                    {
                        return Err("Hook returned conflicting token limits.");
                    }
                    if endpoint == Endpoint::CountTokens
                        && (!value["messages"].is_array()
                            || value.to_string().len() > extra::COUNT_BODY_LIMIT)
                    {
                        return Err("Hook returned an invalid token count request.");
                    }
                    if endpoint == Endpoint::Rerank
                        && (!value["query"].is_string()
                            || value["documents"].as_array().is_none_or(Vec::is_empty)
                            || is_stream(value)
                            || value.to_string().len() > extra::COUNT_BODY_LIMIT)
                    {
                        return Err("Hook returned an invalid rerank request.");
                    }
                    if endpoint == Endpoint::AudioSpeech
                        && (value["input"].as_str().is_none_or(str::is_empty)
                            || value.to_string().len() > extra::COUNT_BODY_LIMIT)
                    {
                        return Err("Hook returned an invalid speech request.");
                    }
                    let model = model_of(value).unwrap_or_default();
                    if Some(&model) != requested_model.as_ref() {
                        let index = st.resolution_index.read().expect("resolution index");
                        let resolution =
                            crate::resolve::resolve(&table, &catalog, &index, &model, access);
                        if resolution.owner(&table, &catalog, access).is_none()
                            || !resolution.allowed(access, &model, &catalog)
                        {
                            return Err("Hook proposed a model not allowed for this key.");
                        }
                    }
                    Ok(())
                })
                .await;
            if let Err(response) = result {
                let entry = NewEntry {
                    semantic_cache_hit: false,
                    semantic_cache_embed: internal_embedding,
                    route_rule: matched_rule.clone(),
                    cache_hit: false,
                    ts_ms,
                    user_id: auth.user_id,
                    key_id: auth.key_id,
                    team_id: auth.team_id,
                    provider: first.provider.name.clone(),
                    subscription: false,
                    endpoint: endpoint.name().into(),
                    requested_model: requested_model.clone(),
                    model: first.model.clone(),
                    status: response.status().as_u16(),
                    streamed: is_stream(value),
                    usage: Usage::default(),
                    reported_cost_micros: None,
                    quote: Default::default(),
                    estimated: false,
                    latency_ms: started.elapsed().as_millis() as i64,
                    ttft_ms: None,
                    client_aborted: false,
                    provider_key: None,
                    attempts: 0,
                    session_affinity: None,
                };
                let state = st.clone();
                let session = guardrails.clone();
                st.spawn_tracked(async move {
                    session.wait().await;
                    if let Some(id) = crate::ledger_spool::PendingEntry::new(entry.clone())
                        .persist_reserved(&state, hook_admission)
                        .await
                    {
                        session.hooks.ledger(id, &entry).await;
                        let _ = session.ledger(&state.db, id).await;
                    }
                });
                return *response;
            }
            drop(hook_admission);
            if audio_upload.is_none() && model_of(value) != requested_model {
                // Re-enter the same authenticated surface for model resolution
                // and Provider Access. Admission and pre hooks run only once.
                // No body field can replace the original authentication headers.
                let body = Bytes::from(value.to_string());
                return Box::pin(handle_inner_native(
                    st,
                    method,
                    uri,
                    headers,
                    Ok(body),
                    guardrails,
                    native,
                    internal_embedding,
                ))
                .await;
            }
            if audio_upload.is_none() {
                body = Bytes::from(value.to_string());
            }
        }
    }

    let cache_plan = table
        .routes
        .iter()
        .find(|r| Some(r.name.as_str()) == resolved_route)
        .and_then(|r| {
            if !r.cache.enabled || internal_embedding {
                return None;
            }
            if !matches!(
                endpoint,
                Endpoint::ChatCompletions
                    | Endpoint::Messages
                    | Endpoint::Responses
                    | Endpoint::Embeddings
            ) {
                return None;
            }
            let mut identity = serde_json::json!([
                native.as_ref().map(crate::inbound::Context::cache_identity),
                r.name,
                r.strategy,
                r.cache,
                r.deployments
                    .iter()
                    .map(|d| (&d.provider, &d.model, d.weight))
                    .collect::<Vec<_>>(),
                r.fallbacks,
                table
                    .reachable_providers(&catalog, &r.name)
                    .iter()
                    .map(|p| (
                        &p.name,
                        &p.base_url,
                        p.kind,
                        p.auth.as_str(),
                        p.include_usage
                    ))
                    .collect::<Vec<_>>()
            ]);
            if let Some(rule) = &matched_rule {
                identity
                    .as_array_mut()
                    .expect("cache identity array")
                    .extend([serde_json::json!(rule.id), serde_json::json!(rule.side)]);
            }
            let identity = identity.to_string();
            crate::cache::Plan::new(
                &r.cache,
                &identity,
                uri.path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or(uri.path()),
                json_body.as_ref()?,
                &headers,
                auth.key_id,
                auth.team_id,
            )
        });
    let mut cached = if let Some(plan) = &cache_plan {
        match crate::cache::lookup(&st.db, &st.secret_key, plan, &settings.cache).await {
            Ok(entry) => entry.filter(|entry| {
                candidates
                    .iter()
                    .any(|c| c.provider.name == entry.provider && c.model == entry.upstream_model)
            }),
            Err(e) => {
                tracing::warn!(error=%e,"cache lookup failed");
                None
            }
        }
    } else {
        None
    };
    let mut semantic_score = None;
    if cache_plan.as_ref().is_some_and(|p| p.lookup) && cached.is_none() {
        metrics::counter!("llm_cache_misses_total").increment(1);
    }
    let mut semantic_plan = None;
    if cached.is_none()
        && !internal_embedding
        && matches!(
            endpoint,
            Endpoint::ChatCompletions | Endpoint::Messages | Endpoint::Responses
        )
        && let Some(route) = table
            .routes
            .iter()
            .find(|r| Some(r.name.as_str()) == resolved_route)
        && let Some(body) = &json_body
    {
        let identity = json!([
            native
                .as_ref()
                .map(crate::inbound::Context::semantic_identity),
            route.name,
            route.strategy,
            route
                .deployments
                .iter()
                .map(|d| (&d.provider, &d.model, d.weight))
                .collect::<Vec<_>>(),
            route.fallbacks,
            table
                .reachable_providers(&catalog, &route.name)
                .iter()
                .map(|p| (
                    &p.name,
                    &p.base_url,
                    p.kind,
                    p.auth.as_str(),
                    p.include_usage
                ))
                .collect::<Vec<_>>(),
            matched_rule.as_ref().map(|r| (&r.id, &r.side))
        ])
        .to_string();
        if let Some(mut plan) = crate::semantic_cache::Plan::new(
            &route.cache.semantic,
            &identity,
            uri.path_and_query()
                .map(|p| p.as_str())
                .unwrap_or(uri.path()),
            body,
            &headers,
            auth.key_id,
            auth.team_id,
        ) && let Some(embedding) = semantic_embedding(st.clone(), &headers, &plan).await
        {
            plan.bind_embedding(&embedding.identity);
            let vector = embedding.vector;
            match crate::semantic_cache::lookup(
                &st.db,
                &st.secret_key,
                &plan,
                &vector,
                &settings.cache,
            )
            .await
            {
                Ok(Some((entry, score)))
                    if candidates.iter().any(|c| {
                        c.provider.name == entry.provider && c.model == entry.upstream_model
                    }) =>
                {
                    cached = Some(entry);
                    semantic_score = Some(score);
                }
                Err(e) => tracing::warn!(error=%e,"semantic cache lookup failed"),
                _ => {}
            }
            if cached.is_none() && plan.exact.lookup {
                metrics::counter!("llm_semantic_cache_misses_total").increment(1);
            }
            semantic_plan = Some(crate::semantic_cache::WritePlan { plan, vector });
        }
    }
    let mut reservation = None;
    if !simple {
        let mut worst = 0;
        for c in candidates.iter().filter(|_| cached.is_none()) {
            let pricing_model = if c.provider.auth == crate::provider::ProviderAuth::GoogleVertex {
                c.model
                    .as_deref()
                    .and_then(|name| crate::vertex::model(name).ok())
                    .map(|(_, id)| id)
            } else {
                c.model.as_deref()
            };
            let Ok((prepared, target, _)) =
                prepare_inbound_body(c, endpoint, json_body.as_ref(), native.as_ref())
            else {
                continue;
            };
            let len = if extra::audio_upload(endpoint) {
                body.len()
            } else {
                prepared
                    .as_ref()
                    .map_or(body.len(), |v| v.to_string().len())
            };
            let model_id = pricing_model.and_then(|m| catalog.resolve(m)).map(|m| m.id);
            let reserve_output = match crate::models_dev::reservation_output(
                &st.db,
                model_id,
                settings.reserve_output_tokens,
            )
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error=%e,"model output limit lookup failed");
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "api_error",
                        "internal error",
                    );
                }
            };
            worst = worst.max(worst_case_price(
                &catalog,
                auth.team_id,
                target,
                pricing_model,
                prepared.as_ref(),
                len,
                reserve_output,
            ));
        }
        if cached.is_some() {
            worst = settings.cache.hit_price_micros.max(0);
        }
        match crate::budgets::reserve(&st, &auth, worst, ts_ms / 1000).await {
            Ok(Some(r)) => reservation = Some(r),
            Ok(None) => {
                return error(
                    StatusCode::PAYMENT_REQUIRED,
                    "budget_exceeded",
                    "a Virtual Key, User or Team budget does not cover this request",
                );
            }
            Err(e) => {
                tracing::error!(error=%e,"budget check failed");
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "internal error",
                );
            }
        }
    }

    let streamed = endpoint == Endpoint::AudioSpeech || json_body.as_ref().is_some_and(is_stream);
    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let mut trace = crate::observability::RequestTrace::new(
        &st,
        resolved_route,
        auth.key_id,
        &body,
        table.settings.max_attempts,
    )
    .await;
    trace.route_rule = matched_rule;
    let mut ctx = Ctx {
        provider_permit: None,
        route: first.route.clone(),
        guardrails,
        cache_plan,
        semantic_plan,
        semantic_cache_hit: semantic_score.is_some(),
        semantic_cache_embed: internal_embedding,
        cache_hit_price: None,
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
        trace,
        session_affinity: None,
        rate_scopes: scopes.into_iter().map(|(scope, _)| scope).collect(),
        reservation,
    };
    if let Some(entry) = cached
        && let Ok(bytes) = entry.bytes()
    {
        ctx.cache_hit_price = Some(settings.cache.hit_price_micros.max(0));
        ctx.provider = candidates
            .iter()
            .find(|c| c.provider.name == entry.provider && c.model == entry.upstream_model)
            .expect("checked cached provider")
            .provider
            .clone();
        ctx.upstream_model = entry.upstream_model;
        let mut resolution_headers = HeaderMap::new();
        ctx.route = candidates
            .iter()
            .find(|c| c.provider.name == ctx.provider.name && c.model == ctx.upstream_model)
            .expect("cached candidate")
            .route
            .clone();
        ctx.resolution_headers(&mut resolution_headers);
        ctx.finish(Outcome {
            status: 200,
            model: entry.model,
            usage: entry.usage,
            reported_cost_micros: None,
            estimated: entry.estimated,
            ttft_ms: streamed.then_some(0),
            client_aborted: false,
        });
        if semantic_score.is_some() {
            metrics::counter!("llm_semantic_cache_hits_total").increment(1);
            metrics::counter!("llm_semantic_cache_hit_bytes_total").increment(bytes.len() as u64);
        } else {
            metrics::counter!("llm_cache_hits_total").increment(1);
            metrics::counter!("llm_cache_hit_bytes_total").increment(bytes.len() as u64);
        }
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().extend(resolution_headers);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&entry.content_type)
                .unwrap_or(HeaderValue::from_static("application/json")),
        );
        response.headers_mut().insert(
            "x-tokomotive-cache",
            semantic_score.map_or(HeaderValue::from_static("hit"), |score| {
                HeaderValue::from_str(&format!("semantic-hit; similarity={score:.3}"))
                    .expect("similarity header")
            }),
        );
        return response;
    }
    let cache_status = if ctx.cache_plan.as_ref().is_some_and(|p| p.lookup)
        || ctx
            .semantic_plan
            .as_ref()
            .is_some_and(|p| p.plan.exact.lookup)
    {
        "miss"
    } else {
        "bypass"
    };

    // Try each deployment's keys before the next deployment. Nothing has
    // been sent to the client, so connection errors, 429 and 5xx can retry.
    let degraded = !candidates.iter().any(|c| {
        table
            .routes
            .iter()
            .any(|r| r.name == c.route && matches!(r.strategy, crate::config::Strategy::Adaptive))
    }) && candidates
        .iter()
        .all(|c| !st.deployment_available(c, &table.settings));
    let mut upstream = None;
    let mut oauth_retrying = false;
    let mut translation_error = None;
    let mut strip_usage_chunk = false;
    let mut direction = None;
    let mut upstream_endpoint = endpoint;
    let mut previous_route = None;
    let mut skipped_accounts = false;
    let mut limited_candidates = 0;
    let mut retry_at: Option<i64> = None;
    'deployments: for candidate in &candidates {
        if candidate.provider.secret_unavailable {
            ctx.trace.skip(candidate, None, "secret_unavailable");
            ctx.finish(Outcome::failed(503));
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "secret unavailable",
            );
        }
        if ctx.attempts as usize >= table.settings.max_attempts {
            break;
        }
        let Some(permit) = st.acquire_deployment(candidate, &table.settings, degraded) else {
            ctx.trace.skip(candidate, None, "circuit_open");
            continue;
        };
        let provider = &candidate.provider;
        let Some(provider_permit) = provider
            .gate
            .acquire(&provider.name, provider.limits.max_wait_ms.unwrap_or(0))
            .await
        else {
            limited_candidates += 1;
            ctx.trace.skip(candidate, None, "concurrency_limited");
            continue;
        };
        let Some(egress) = provider.egress.as_ref() else {
            continue;
        };
        let oauth = st.oauth.clone().with_egress(egress.clone());
        if endpoint == Endpoint::CountTokens
            && !extra::native_count(provider)
            && !(provider.kind == ProviderKind::Gemini
                && provider.auth == crate::provider::ProviderAuth::ApiKey
                && native.is_some())
        {
            permit.record(true);
            ctx.provider = provider.clone();
            ctx.upstream_model = candidate.model.clone();
            ctx.route = candidate.route.clone();
            return extra::estimate_count(ctx, json_body.as_ref().unwrap());
        }
        let dir = translation(provider.kind, endpoint);
        let (prepared, target_endpoint, strip) =
            match prepare_inbound_body(candidate, endpoint, json_body.as_ref(), native.as_ref()) {
                Ok(v) => v,
                Err(e) => {
                    ctx.trace.skip(candidate, None, "translation");
                    translation_error = Some(e);
                    continue;
                }
            };
        let prepared = if provider.auth == crate::provider::ProviderAuth::GeminiCli {
            let Ok(project) = oauth.code_assist_project(&provider.name).await else {
                ctx.trace.skip(candidate, None, "credentials_invalid");
                continue;
            };
            Some(json!({"model":candidate.model,"project":project,"request":prepared}))
        } else {
            prepared
        };
        let upstream_body = match prepared {
            Some(v) => Bytes::from(serde_json::to_vec(&v).unwrap_or_default()),
            None => body.clone(),
        };
        let mut path = dir.map_or_else(
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
        let codex = provider.auth == crate::provider::ProviderAuth::Codex;
        let unofficial = matches!(
            provider.auth,
            crate::provider::ProviderAuth::GithubCopilot
                | crate::provider::ProviderAuth::QwenCode
                | crate::provider::ProviderAuth::KimiCode
        );
        if unofficial
            && (method != Method::POST
                || !subscription_path_allowed(endpoint, path_q)
                || !matches!(
                    target_endpoint,
                    Endpoint::ChatCompletions | Endpoint::Responses | Endpoint::Messages
                )
                || (provider.auth == crate::provider::ProviderAuth::QwenCode
                    && target_endpoint != Endpoint::ChatCompletions))
        {
            translation_error = Some(translate::TranslateError {
                message: "This subscription accepts only canonical model request paths".into(),
            });
            ctx.trace.skip(candidate, None, "translation");
            continue;
        }
        let vertex = provider.auth == crate::provider::ProviderAuth::GoogleVertex;
        if vertex {
            if method != Method::POST
                || !subscription_path_allowed(endpoint, path_q)
                || !matches!(endpoint, Endpoint::ChatCompletions | Endpoint::Messages)
            {
                translation_error = Some(translate::TranslateError {
                    message:
                        "Vertex accepts only canonical Chat Completions and Messages POST paths"
                            .into(),
                });
                ctx.trace.skip(candidate, None, "translation");
                continue;
            }
            path = match crate::vertex::model_path(
                &provider.project_id,
                &provider.region,
                candidate.model.as_deref().unwrap_or_default(),
                streamed,
            ) {
                Ok(path) => path,
                Err(e) => {
                    translation_error = Some(translate::TranslateError {
                        message: e.to_string(),
                    });
                    ctx.trace.skip(candidate, None, "translation");
                    continue;
                }
            };
        } else if matches!(provider.kind, ProviderKind::Gemini | ProviderKind::Bedrock) {
            if method != Method::POST || !subscription_path_allowed(endpoint, path_q) {
                translation_error = Some(translate::TranslateError {
                    message:
                        "Gemini accepts only canonical Chat Completions and Messages POST paths"
                            .into(),
                });
                ctx.trace.skip(candidate, None, "translation");
                continue;
            }
            path = match (if provider.kind == ProviderKind::Bedrock {
                crate::translate_bedrock::path
            } else {
                crate::translate_gemini::path
            })(candidate.model.as_deref().unwrap_or_default(), streamed)
            {
                Ok(path) => path,
                Err(error) => {
                    translation_error = Some(error);
                    ctx.trace.skip(candidate, None, "translation");
                    continue;
                }
            };
            if provider.auth == crate::provider::ProviderAuth::GeminiCli {
                path = if streamed {
                    "/v1internal:streamGenerateContent?alt=sse"
                } else {
                    "/v1internal:generateContent"
                }
                .into();
            }
            if provider.kind == ProviderKind::Gemini
                && let Some(native) = &native
                && native.is_gemini_aux()
                && provider.auth == crate::provider::ProviderAuth::ApiKey
            {
                path = match native
                    .upstream_path(candidate.model.as_deref().unwrap_or_default(), streamed)
                {
                    Ok(path) => path,
                    Err(error) => {
                        translation_error = Some(error);
                        ctx.trace.skip(candidate, None, "translation");
                        continue;
                    }
                };
            }
        }
        if provider.kind == ProviderKind::Cohere {
            if method != Method::POST || !subscription_path_allowed(endpoint, path_q) {
                translation_error = Some(translate::TranslateError {
                    message:
                        "Cohere accepts only canonical Chat Completions and Messages POST paths"
                            .into(),
                });
                ctx.trace.skip(candidate, None, "translation");
                continue;
            }
            path = if endpoint == Endpoint::Rerank {
                "/v2/rerank"
            } else {
                "/v2/chat"
            }
            .into();
        }
        if codex && (method != Method::POST || !subscription_path_allowed(endpoint, path_q)) {
            translation_error = Some(translate::TranslateError {
                message: "Codex accepts only canonical model request paths".into(),
            });
            ctx.trace.skip(candidate, None, "translation");
            continue;
        }
        if codex {
            path = "/responses".into();
        }
        if !codex
            && !vertex
            && provider.auth != crate::provider::ProviderAuth::GeminiCli
            && provider.auth != crate::provider::ProviderAuth::ApiKey
            && !subscription_path_allowed(target_endpoint, &path)
        {
            translation_error = Some(translate::TranslateError {
                message: format!(
                    "provider {} accepts only model requests on this path",
                    provider.name
                ),
            });
            ctx.trace.skip(candidate, None, "translation");
            continue;
        }
        if provider.auth == crate::provider::ProviderAuth::GithubCopilot
            && target_endpoint != Endpoint::Messages
        {
            path = path.strip_prefix("/v1").unwrap_or(&path).into();
        }
        if provider.auth == crate::provider::ProviderAuth::KimiCode
            && target_endpoint == Endpoint::Messages
        {
            path = "/v1/messages?beta=true".into();
        }
        let mut order = quota_schedule.key_order(provider, chrono::Utc::now().timestamp());
        if let Some(session) = &session {
            session.order_keys(provider, &mut order);
        }
        order.retain(|&idx| {
            if let Some(skip) = quota_schedule.skip(
                provider,
                idx,
                chrono::Utc::now().timestamp(),
                st.settings().router.quota_exhausted_percent,
                table.settings.cooldown_secs,
            ) {
                ctx.trace.skip(candidate, idx, skip.reason);
                metrics::counter!("tokomotive_account_skips_total", "reason" => skip.reason)
                    .increment(1);
                skipped_accounts = true;
                if let Some(session) = &mut session {
                    session.unavailable(provider, idx);
                }
                if let Some(t) = skip.until {
                    retry_at = Some(retry_at.map_or(t, |old| old.min(t)));
                }
                return false;
            }
            true
        });
        let provider_cap = provider
            .limits
            .max_retries
            .map(|n| n.saturating_add(1))
            .unwrap_or(usize::MAX);
        let tries = provider
            .limits
            .max_retries
            .map(|n| n.saturating_add(1))
            .unwrap_or(order.len())
            .min(table.settings.max_attempts);
        let mut provider_attempts = 0;
        for idx in order.iter().copied().cycle().take(tries) {
            if provider_attempts >= provider_cap {
                break;
            }
            if ctx.attempts as usize >= table.settings.max_attempts {
                break 'deployments;
            }
            if provider_attempts > 0 {
                provider.limits.backoff(provider_attempts).await;
            }
            provider_attempts += 1;
            if ctx.attempts > 0 {
                metrics::counter!("llm_upstream_retries_total", "provider" => ctx.provider.name.clone()).increment(1);
            }
            if let Some(from) = previous_route.take() {
                metrics::counter!("llm_upstream_fallbacks_total", "from" => from, "to" => candidate.route.clone()).increment(1);
            }
            ctx.provider = provider.clone();
            ctx.upstream_model = candidate.model.clone();
            ctx.route = candidate.route.clone();
            ctx.attempts += 1;
            ctx.provider_key = idx.map(|i| i as i64);
            let mut attempt = ctx.trace.begin(candidate, idx);
            let mut attempt_started = Instant::now();
            strip_usage_chunk = strip;
            direction = dir;
            upstream_endpoint = target_endpoint;
            let flow = crate::oauth::Flow::from_auth(provider.auth);
            let oauth_token = if let Some(flow) = flow {
                let refreshed = oauth.token(&provider.name, flow, None).await;
                oauth.emit_refresh_events(&st).await;
                match refreshed {
                    Ok(token) => {
                        crate::quota::emit_health_events(&st).await;
                        Some(token)
                    }
                    Err(e) => {
                        let retrying = e.is::<crate::oauth::RefreshRetrying>();
                        oauth_retrying |= retrying;
                        ctx.trace.result(
                            attempt,
                            Some(if retrying { 503 } else { 401 }),
                            attempt_started,
                        );
                        if !retrying {
                            permit.record(false);
                            ctx.trace.health(attempt, false);
                        }
                        upstream = None;
                        break;
                    }
                }
            } else {
                None
            };
            let key = oauth_token
                .as_deref()
                .unwrap_or_else(|| idx.map(|i| provider.keys[i].as_str()).unwrap_or(""));
            let Ok(base) = st
                .oauth
                .inference_resource(&provider.name, provider.auth, &provider.base_url)
                .await
            else {
                permit.record(false);
                ctx.trace.health(attempt, false);
                break;
            };
            // API-key Providers keep the version-aware join from the presets
            // (an `/api/v1` base and a `/v1/...` path do not double `/v1`).
            let url = if matches!(
                provider.auth,
                crate::provider::ProviderAuth::ApiKey
                    | crate::provider::ProviderAuth::QwenCode
                    | crate::provider::ProviderAuth::KimiCode
            ) {
                crate::provider::upstream_url(&base, &path)
            } else {
                format!("{}{}", base.trim_end_matches('/'), path)
            };
            let Some(req) = build_request(
                &st,
                provider,
                key,
                &method,
                &url,
                &headers,
                RequestHints {
                    anthropic: anthropic_hint,
                    body: &upstream_body,
                },
            )
            .await
            else {
                permit.record(false);
                ctx.trace.health(attempt, false);
                break;
            };
            upstream = None;
            let send = |req: reqwest::RequestBuilder| {
                if let Some(upload) = &audio_upload {
                    req.multipart(upload.form(candidate.model.as_deref()))
                } else {
                    req.body(upstream_body.clone())
                }
            };
            let timeout_ms = provider
                .limits
                .request_timeout_ms
                .unwrap_or_else(|| st.settings().upstream_timeout_secs.saturating_mul(1000));
            let send_request = |req: reqwest::RequestBuilder| async move {
                let req = if !streamed && !codex {
                    req.timeout(std::time::Duration::from_millis(timeout_ms.max(1)))
                } else {
                    req
                };
                tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms.max(1)),
                    send(req).send(),
                )
                .await
            };
            let mut result = match send_request(req).await {
                Ok(result) => result,
                Err(_) => {
                    ctx.trace.result(attempt, None, attempt_started);
                    ctx.trace.classify_last("request_timeout");
                    metrics::counter!("tokomotive_provider_timeouts_total", "provider" => provider.name.clone(), "kind" => "request").increment(1);
                    permit.record(false);
                    ctx.trace.health(attempt, false);
                    if permit.is_probe() {
                        break;
                    }
                    continue;
                }
            };
            ctx.trace.result(
                attempt,
                result.as_ref().ok().map(|r| r.status().as_u16()),
                attempt_started,
            );
            oauth
                .observe_inference(&st, provider, idx, key, &result)
                .await;
            if let Some(flow) = flow
                && result
                    .as_ref()
                    .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED)
                && (ctx.attempts as usize) < table.settings.max_attempts
                && provider_attempts < provider_cap
            {
                let refreshed = oauth.token(&provider.name, flow, Some(key)).await;
                oauth.emit_refresh_events(&st).await;
                match refreshed {
                    Ok(token) => {
                        crate::quota::emit_health_events(&st).await;
                        ctx.attempts += 1;
                        provider.limits.backoff(provider_attempts).await;
                        provider_attempts += 1;
                        attempt = ctx.trace.begin(candidate, idx);
                        attempt_started = Instant::now();
                        metrics::counter!("llm_upstream_retries_total", "provider" => provider.name.clone()).increment(1);
                        let Ok(base) = st
                            .oauth
                            .inference_resource(&provider.name, provider.auth, &provider.base_url)
                            .await
                        else {
                            permit.record(false);
                            ctx.trace.health(attempt, false);
                            break;
                        };
                        let url = if matches!(
                            provider.auth,
                            crate::provider::ProviderAuth::QwenCode
                                | crate::provider::ProviderAuth::KimiCode
                        ) {
                            crate::provider::upstream_url(&base, &path)
                        } else {
                            format!("{}{}", base.trim_end_matches('/'), path)
                        };
                        let Some(req) = build_request(
                            &st,
                            provider,
                            &token,
                            &method,
                            &url,
                            &headers,
                            RequestHints {
                                anthropic: anthropic_hint,
                                body: &upstream_body,
                            },
                        )
                        .await
                        else {
                            permit.record(false);
                            ctx.trace.health(attempt, false);
                            break;
                        };
                        result = match send_request(req).await {
                            Ok(result) => result,
                            Err(_) => {
                                ctx.trace.result(attempt, None, attempt_started);
                                ctx.trace.classify_last("request_timeout");
                                metrics::counter!("tokomotive_provider_timeouts_total", "provider" => provider.name.clone(), "kind" => "request").increment(1);
                                permit.record(false);
                                ctx.trace.health(attempt, false);
                                break;
                            }
                        };
                        ctx.trace.result(
                            attempt,
                            result.as_ref().ok().map(|r| r.status().as_u16()),
                            attempt_started,
                        );
                        oauth
                            .observe_inference(&st, provider, idx, &token, &result)
                            .await;
                        if result
                            .as_ref()
                            .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED)
                        {
                            let _ = oauth.reject(&provider.name, flow, &token).await;
                            oauth.emit_refresh_events(&st).await;
                        }
                    }
                    Err(e) => {
                        let retrying = e.is::<crate::oauth::RefreshRetrying>();
                        oauth_retrying |= retrying;
                        upstream = None;
                        if !retrying {
                            permit.record(false);
                            ctx.trace.health(attempt, false);
                        }
                        break;
                    }
                }
            }
            match result {
                Ok(r) => {
                    let credentials_invalid =
                        matches!(r.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
                    let rate_limited = r.status() == StatusCode::TOO_MANY_REQUESTS;
                    let known_reset = rate_limited
                        && crate::quota::retry_at(r.headers(), chrono::Utc::now().timestamp())
                            .is_some();
                    let healthy = !retryable(r.status()) && !credentials_invalid;
                    permit.record(healthy || credentials_invalid || known_reset);
                    ctx.trace
                        .health(attempt, healthy || credentials_invalid || known_reset);
                    if (credentials_invalid || rate_limited)
                        && let Some(session) = &mut session
                    {
                        session.unavailable(provider, idx);
                    }
                    upstream = Some(r);
                    if healthy {
                        ctx.provider_permit = Some(provider_permit);
                        if upstream.as_ref().is_some_and(|r| r.status().is_success())
                            && let Some(session) = &session
                        {
                            ctx.session_affinity =
                                Some(st.pin_session(&table, session, candidate, idx));
                        }
                        break 'deployments;
                    }
                }
                Err(e) => {
                    if e.is_timeout() {
                        let kind = if e.is_connect() { "connect" } else { "request" };
                        ctx.trace.classify_last(if e.is_connect() {
                            "connect_timeout"
                        } else {
                            "request_timeout"
                        });
                        metrics::counter!("tokomotive_provider_timeouts_total", "provider" => provider.name.clone(), "kind" => kind).increment(1);
                    }
                    permit.record(false);
                    ctx.trace.health(attempt, false);
                    egress.connect_error(&provider.name, &e);
                    tracing::warn!(provider = %provider.name, class = crate::egress::error_class(&e), "upstream request failed");
                }
            }
            if permit.is_probe() {
                break;
            }
        }
        previous_route = Some(candidate.route.clone());
    }
    let Some(upstream) = upstream else {
        if limited_candidates == candidates.len() && !candidates.is_empty() {
            ctx.finish(Outcome::failed(429));
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")], axum::Json(if endpoint == Endpoint::Messages {
                json!({"type":"error","error":{"type":"rate_limit_error","message":"All Providers are concurrency_limited"}})
            } else {
                json!({"error":{"type":"rate_limit_error","message":"All Providers are concurrency_limited"}})
            })).into_response();
        }
        if oauth_retrying {
            ctx.finish(Outcome::failed(503));
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Sign-in refresh is retrying",
            );
        }
        if ctx.attempts == 0 && skipped_accounts {
            ctx.finish(Outcome::failed(429));
            let body = if endpoint == Endpoint::Messages {
                json!({"type":"error","error":{"type":"rate_limit_error","message":"All accounts are skipped"}})
            } else {
                json!({"error":{"type":"rate_limit_error","message":"All accounts are skipped"}})
            };
            let seconds = retry_at
                .map(|t| t.saturating_sub(chrono::Utc::now().timestamp()).max(1))
                .unwrap_or(table.settings.cooldown_secs.max(1) as i64);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, seconds.to_string())],
                axum::Json(body),
            )
                .into_response();
        }
        if ctx.attempts == 0
            && let Some(e) = translation_error
        {
            ctx.finish(Outcome::failed(400));
            return error(StatusCode::BAD_REQUEST, "invalid_request_error", &e.message);
        }
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream request failed",
        );
    };

    let status = upstream.status();
    if status.is_redirection() {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "upstream redirect rejected",
        );
    }
    let mut out_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if !skip_header(name)
            && name != header::CONTENT_ENCODING
            && (native.is_none() || crate::inbound::safe_upstream_header(name))
        {
            out_headers.append(name.clone(), value.clone());
        }
    }
    out_headers.insert("x-tokomotive-cache", HeaderValue::from_static(cache_status));
    if status.is_success() {
        ctx.resolution_headers(&mut out_headers);
    }
    if endpoint == Endpoint::AudioSpeech && status.is_success() {
        let characters = json_body
            .as_ref()
            .and_then(|v| v["input"].as_str())
            .map_or(0, |s| s.chars().count()) as i64;
        return extra::speech_response(ctx, upstream, out_headers, characters);
    }
    // Native errors must reach the status-mapped sanitizer even when the
    // Provider labels its error body as a stream.
    let is_sse = (native.is_none() || status.is_success())
        && upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.starts_with("text/event-stream")
                    || (ctx.provider.kind == ProviderKind::Bedrock
                        && status.is_success()
                        && v.starts_with("application/vnd.amazon.eventstream"))
            });

    if is_sse && (extra::audio_upload(endpoint) || endpoint == Endpoint::Rerank) {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "this endpoint requires a non-streaming upstream response",
        );
    }
    if is_sse {
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let codex = ctx.provider.auth == crate::provider::ProviderAuth::Codex;
        if codex && !streamed && status.is_success() {
            return collect_codex(ctx, upstream, out_headers).await;
        }
        let translator = direction.map(|dir| {
            out_headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            ProxyStream::Chat(
                StreamTranslator::new(dir, ctx.upstream_model.as_deref().unwrap_or_default())
                    .with_usage(
                        json_body
                            .as_ref()
                            .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                    ),
            )
        });
        let translator = if ctx.provider.kind == ProviderKind::Gemini {
            Some(ProxyStream::Gemini(
                crate::translate_gemini::GeminiStream::new(
                    endpoint,
                    ctx.upstream_model.as_deref().unwrap_or_default(),
                    json_body
                        .as_ref()
                        .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                ),
            ))
        } else if ctx.provider.kind == ProviderKind::Bedrock {
            Some(ProxyStream::Bedrock(
                crate::translate_bedrock::BedrockStream::new(
                    endpoint,
                    ctx.upstream_model.as_deref().unwrap_or_default(),
                    json_body
                        .as_ref()
                        .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                ),
            ))
        } else if ctx.provider.kind == ProviderKind::Cohere {
            Some(ProxyStream::Cohere(
                crate::translate_cohere::CohereStream::new(
                    endpoint,
                    ctx.upstream_model.as_deref().unwrap_or_default(),
                    json_body
                        .as_ref()
                        .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                ),
            ))
        } else if codex && endpoint != Endpoint::Responses {
            Some(ProxyStream::Responses(
                ResponseStream::new(
                    ctx.upstream_model.as_deref().unwrap_or_default(),
                    endpoint == Endpoint::Messages,
                )
                .with_usage(
                    json_body
                        .as_ref()
                        .is_some_and(|v| v["stream_options"]["include_usage"] == true),
                ),
            ))
        } else {
            translator
        };
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

    let bytes = match read_capped(upstream, MAX_UPSTREAM_BODY, &ctx.provider.name).await {
        Ok(b) => b,
        Err(e) => {
            if e.is::<crate::provider_limits::RequestTimeout>() {
                ctx.trace.classify_last("request_timeout");
                metrics::counter!("tokomotive_provider_timeouts_total", "provider" => ctx.provider.name.clone(), "kind" => "request").increment(1);
            }
            tracing::warn!(provider = %ctx.provider.name, class = "body", "upstream body read failed");
            ctx.finish(Outcome::failed(502));
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream response failed",
            );
        }
    };
    ctx.trace.response(&bytes);
    let parsed: Option<Value> = serde_json::from_slice::<Value>(&bytes).ok().map(|v| {
        if ctx.provider.auth == crate::provider::ProviderAuth::GeminiCli && status.is_success() {
            v.get("response").cloned().unwrap_or(Value::Null)
        } else {
            v
        }
    });
    let kind = ctx.provider.kind;
    if kind == ProviderKind::Cohere
        && endpoint != Endpoint::Rerank
        && status.is_success()
        && parsed.as_ref().is_none_or(|v| {
            !v["message"].is_object()
                || v["finish_reason"]
                    .as_str()
                    .and_then(crate::translate_cohere::stop)
                    .is_none()
        })
    {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "invalid or failed Cohere response",
        );
    }
    if kind == ProviderKind::Bedrock
        && status.is_success()
        && parsed
            .as_ref()
            .is_none_or(|v| !v["output"]["message"]["content"].is_array())
    {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "invalid Bedrock response",
        );
    }
    if kind == ProviderKind::Gemini
        && !native
            .as_ref()
            .is_some_and(crate::inbound::Context::is_gemini_aux)
        && status.is_success()
        && parsed.as_ref().is_none_or(|v| {
            !v["candidates"].is_array() && !v["promptFeedback"]["blockReason"].is_string()
        })
    {
        ctx.finish(Outcome::failed(502));
        return error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "invalid Gemini response",
        );
    }
    let usage = (endpoint == Endpoint::CountTokens)
        .then_some(Usage::default())
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|v| kind.usage_from_json(upstream_endpoint, v))
        });
    let model = parsed.as_ref().and_then(model_of);
    let estimated = usage.is_none()
        && status.is_success()
        && endpoint.is_accounted()
        && endpoint != Endpoint::CountTokens;
    let (usage, estimated) = if extra::audio_upload(endpoint) && status.is_success() {
        let (audio, estimate) =
            extra::audio_usage(parsed.as_ref(), audio_upload.as_ref().unwrap().file_bytes);
        (Some(audio), estimate)
    } else if endpoint == Endpoint::Embeddings
        && kind == ProviderKind::Gemini
        && native.is_some()
        && status.is_success()
        && usage.is_none()
    {
        let bytes = json_body
            .as_ref()
            .and_then(|v| v["input"].as_array())
            .map_or(0, |a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::len)
                    .sum::<usize>()
            });
        (
            Some(Usage {
                input: bytes.div_ceil(4) as i64,
                ..Default::default()
            }),
            true,
        )
    } else {
        (usage, estimated)
    };
    let upstream_model = ctx.upstream_model.clone();
    let outcome = Outcome {
        status: status.as_u16(),
        model,
        usage: usage.unwrap_or_else(|| {
            if status.is_success() && endpoint == Endpoint::Rerank {
                Usage {
                    service_units: body.len() as i64,
                    ..Default::default()
                }
            } else {
                Usage::default()
            }
        }),
        reported_cost_micros: parsed
            .as_ref()
            .and_then(crate::provider::reported_cost_micros),
        estimated,
        ttft_ms: None,
        client_aborted: false,
    };

    let bytes = if native.is_some() && !status.is_success() {
        // Sanitize at the upstream boundary so local validation and policy
        // errors can retain their useful messages in the native adapters.
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Bytes::from(translate::translate_provider_error(
            if endpoint == Endpoint::Messages {
                Direction::MessagesToChat
            } else {
                Direction::ChatToMessages
            },
            status.as_u16(),
            &bytes,
        ))
    } else if !status.is_success()
        && (endpoint == Endpoint::Rerank
            || extra::audio_upload(endpoint)
            || endpoint == Endpoint::AudioSpeech)
    {
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Bytes::from(translate::translate_provider_error(
            Direction::ChatToMessages,
            status.as_u16(),
            &bytes,
        ))
    } else if kind == ProviderKind::Gemini
        && status.is_success()
        && native
            .as_ref()
            .is_some_and(crate::inbound::Context::is_gemini_aux)
    {
        Bytes::from(
            native
                .as_ref()
                .unwrap()
                .auxiliary_response(parsed.as_ref().unwrap_or(&Value::Null))
                .unwrap()
                .to_string(),
        )
    } else if endpoint != Endpoint::Rerank
        && matches!(
            kind,
            ProviderKind::Gemini | ProviderKind::Bedrock | ProviderKind::Cohere
        )
    {
        out_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if status.is_success() {
            Bytes::from(
                (if kind == ProviderKind::Bedrock {
                    crate::translate_bedrock::response
                } else if kind == ProviderKind::Cohere {
                    crate::translate_cohere::response
                } else {
                    crate::translate_gemini::response
                })(
                    endpoint,
                    parsed.as_ref().unwrap_or(&Value::Null),
                    upstream_model.as_deref().unwrap_or_default(),
                )
                .to_string(),
            )
        } else {
            let dir = if endpoint == Endpoint::Messages {
                Direction::MessagesToChat
            } else {
                Direction::ChatToMessages
            };
            // Native client errors are sanitized above. Non-native Gemini
            // clients retain Provider messages for compatibility. Bedrock
            // signature errors can echo the signed session token.
            Bytes::from(if kind == ProviderKind::Gemini {
                translate::translate_error(dir, status.as_u16(), &bytes)
            } else {
                translate::translate_provider_error(dir, status.as_u16(), &bytes)
            })
        }
    } else if let Some(dir) = direction {
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
    let bytes = if status.is_success()
        && let (Some(native), Some(raw)) = (&native, &parsed)
    {
        native.retain_response(kind, raw, &bytes)
    } else {
        bytes
    };
    let write = ctx.cache_write(outcome.usage, outcome.model.clone(), outcome.estimated);
    // Record upstream usage before any cancellable cache I/O.
    ctx.finish(outcome);
    if let Some(write) = write
        && status == StatusCode::OK
        && crate::cache::complete_json(endpoint.name(), &bytes)
    {
        write.store(&bytes, "application/json").await;
    }
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

fn retryable(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error()
}

struct RequestHints<'a> {
    anthropic: bool,
    body: &'a [u8],
}

fn copilot_metadata(url: &str, body: &[u8]) -> (bool, bool) {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return (false, false);
    };
    let has = |parts: &Value, kind: &str| {
        parts
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == kind))
    };
    if let Some(input) = body["input"].as_array() {
        return (
            input.last().is_none_or(|item| item["role"] != "user"),
            input.iter().any(|item| {
                has(&item["content"], "input_image") || has(&item["output"], "input_image")
            }),
        );
    }
    let Some(messages) = body["messages"].as_array() else {
        return (false, false);
    };
    let last = messages.last();
    if url.contains("completions") {
        return (
            last.is_none_or(|m| m["role"] != "user")
                || messages
                    .len()
                    .checked_sub(2)
                    .is_some_and(|i| messages[i]["role"] == "tool"),
            messages.iter().any(|m| has(&m["content"], "image_url")),
        );
    }
    (
        last.is_none_or(|m| {
            m["role"] != "user"
                || !m["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] != "tool_result"))
        }),
        messages.iter().any(|m| {
            m["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "image"
                        || (part["type"] == "tool_result" && has(&part["content"], "image"))
                })
            })
        }),
    )
}

async fn build_request(
    st: &AppState,
    provider: &Provider,
    key: &str,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    hints: RequestHints<'_>,
) -> Option<reqwest::RequestBuilder> {
    let http = st.snapshot_http(provider).await.ok()?;
    if provider.kind == ProviderKind::Cohere {
        return Some(
            http.request(method.clone(), url)
                .bearer_auth(key)
                .header("content-type", "application/json"),
        );
    }
    if provider.kind == ProviderKind::Bedrock {
        if method != Method::POST {
            return None;
        }
        let region =
            crate::sigv4::region_from_base(provider.base_url.trim_end_matches('/')).ok()?;
        let prefix = format!("https://{}", crate::sigv4::host(region).ok()?);
        let path = url.strip_prefix(&prefix)?;
        let creds = crate::settings::bedrock_credentials(&st.db, &st.secret_key, &provider.name)
            .await
            .ok()?;
        return crate::sigv4::request(
            &http,
            &creds,
            region,
            path,
            hints.body,
            &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        )
        .ok();
    }
    let mut req = http.request(method.clone(), url);
    for (name, value) in headers.iter() {
        if !skip_header(name)
            && !(matches!(
                provider.auth,
                crate::provider::ProviderAuth::GithubCopilot
                    | crate::provider::ProviderAuth::QwenCode
                    | crate::provider::ProviderAuth::KimiCode
                    | crate::provider::ProviderAuth::GeminiCli
                    | crate::provider::ProviderAuth::GoogleVertex
            ) && (matches!(
                name.as_str(),
                "user-agent"
                    | "cookie"
                    | "openai-organization"
                    | "openai-project"
                    | "editor-version"
                    | "editor-plugin-version"
                    | "copilot-integration-id"
                    | "openai-intent"
                    | "x-github-api-version"
                    | "x-initiator"
                    | "x-interaction-type"
                    | "x-interaction-id"
            ) || name.as_str().starts_with("x-msh-")
                || name.as_str().starts_with("x-dashscope-")
                || name.as_str().starts_with("copilot-")))
            && !(provider.auth == crate::provider::ProviderAuth::Codex
                && matches!(
                    name.as_str(),
                    "originator"
                        | "chatgpt-account-id"
                        | "session-id"
                        | "thread-id"
                        | "x-client-request-id"
                        | "version"
                        | "user-agent"
                        | "accept"
                        | "cookie"
                        | "openai-organization"
                        | "openai-project"
                ))
            && !(provider.kind != ProviderKind::Anthropic
                && name.as_str().starts_with("anthropic-"))
            && !(provider.auth == crate::provider::ProviderAuth::Claude
                && matches!(
                    name.as_str(),
                    "anthropic-beta" | "user-agent" | "anthropic-version"
                ))
        {
            req = req.header(name, value);
        }
    }
    if provider.auth == crate::provider::ProviderAuth::Claude {
        return Some(
            req.bearer_auth(key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", crate::oauth::CLAUDE_BETAS)
                .header("user-agent", crate::oauth::CLAUDE_UA),
        );
    }
    if provider.auth == crate::provider::ProviderAuth::Codex {
        let account = st.oauth.codex_account(&provider.name).await.ok()?;
        let session = crate::oauth::session_id();
        return Some(
            req.bearer_auth(key)
                .header("chatgpt-account-id", account)
                .header("originator", "codex_cli_rs")
                .header("session-id", &session)
                .header("thread-id", &session)
                .header("x-client-request-id", &session)
                .header("version", env!("CARGO_PKG_VERSION"))
                .header(
                    "user-agent",
                    concat!(
                        "codex_cli_rs/",
                        env!("CARGO_PKG_VERSION"),
                        " (Linux; tokomotive)"
                    ),
                )
                .header("accept", "text/event-stream"),
        );
    }
    let req = if provider.auth == crate::provider::ProviderAuth::ApiKey {
        provider.key_header.apply(req, key)
    } else {
        req.bearer_auth(key)
    };
    let mut req = req;
    match provider.auth {
        crate::provider::ProviderAuth::GithubCopilot => {
            let (agent, vision) = copilot_metadata(url, hints.body);
            req = req
                .header("user-agent", "Tokomotive/0.1.0")
                .header("openai-intent", "conversation-edits")
                .header("x-github-api-version", "2026-08-01")
                .header("x-initiator", if agent { "agent" } else { "user" })
                .header("x-interaction-type", "conversation-agent")
                .header("x-interaction-id", crate::oauth::session_id());
            if vision {
                req = req.header("copilot-vision-request", "true");
            }
        }
        crate::provider::ProviderAuth::QwenCode => {
            req = req
                .header("user-agent", "Tokomotive/0.1.0")
                .header("x-dashscope-authtype", "qwen-oauth")
                .header("x-dashscope-useragent", "Tokomotive/0.1.0");
        }
        crate::provider::ProviderAuth::KimiCode => {
            let device = st
                .oauth
                .device_id(&provider.name, crate::oauth::Flow::KimiCode)
                .await
                .ok()?;
            for (k, v) in crate::oauth::kimi_headers(&device) {
                req = req.header(k, v);
            }
        }
        _ => {}
    }
    Some(
        if provider.kind == ProviderKind::Anthropic && !hints.anthropic {
            req.header("anthropic-version", "2023-06-01")
        } else {
            req
        },
    )
}

/// The Codex backend only streams. Collect the final Response for JSON clients
/// with the same usage accounting as streaming clients, without buffering deltas.
async fn collect_codex(
    mut ctx: Ctx,
    upstream: reqwest::Response,
    mut headers: HeaderMap,
) -> Response {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let st = ctx.st.clone();
    let anthropic = ctx.endpoint == Endpoint::Messages;
    // Keep Ctx (including its Reservation) alive after the handler is cancelled.
    // finish records TPM and metrics, then holds the Reservation for the write.
    st.spawn_tracked(async move {
        let mut decoder = ResponseStream::new("", false);
        let mut tracker = ProviderKind::Openai.new_stream_tracker(Endpoint::Responses, false);
        let mut stream = upstream.bytes_stream();
        let mut failed = false;
        while let Some(chunk) = match crate::provider_limits::next_chunk(&mut stream, ctx.provider.limits.stream_idle_timeout_ms).await {
            Ok(item) => item,
            Err(_) => {
                ctx.trace.classify_last("stream_idle_timeout");
                metrics::counter!("tokomotive_provider_timeouts_total", "provider" => ctx.provider.name.clone(), "kind" => "stream_idle").increment(1);
                failed = true;
                None
            }
        } {
            let Ok(chunk) = chunk else {
                failed = true;
                break;
            };
            tracker.feed(&chunk);
            decoder.feed(&chunk);
            ctx.trace.response(&chunk);
        }
        tracker.finish();
        decoder.finish();
        let response = decoder.final_response.take();
        let failed = failed || decoder.failed || response.is_none();
        let endpoint = ctx.endpoint;
        let status = if failed {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        };
        let make_response = async {
            let Some(value) = response.filter(|_| !failed) else {
                return formatted_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Responses upstream failed",
                    endpoint == Endpoint::Messages,
                );
            };
            let value = match endpoint {
                Endpoint::ChatCompletions => translate_responses::response(&value),
                Endpoint::Messages => translate::translate_response(
                    Direction::MessagesToChat,
                    &translate_responses::response(&value),
                ),
                _ => value,
            };
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let bytes = value.to_string();
            if let Some(write) = ctx.cache_write(
                tracker.usage.unwrap_or_default(),
                tracker.model.clone(),
                !tracker.usage_final,
            ) && !tx.is_closed()
                && crate::cache::complete_json(endpoint.name(), bytes.as_bytes())
            {
                write.store(bytes.as_bytes(), "application/json").await;
            }
            let mut response = Response::new(Body::from(bytes));
            *response.headers_mut() = headers;
            response
        };
        let response = make_response.await;
        let client_aborted = tx.send(response).is_err();
        ctx.finish(Outcome {
            status: status.as_u16(),
            model: tracker.model.take(),
            usage: tracker.usage.unwrap_or_default(),
            reported_cost_micros: tracker.reported_cost_micros,
            estimated: !tracker.usage_final,
            ttft_ms: None,
            client_aborted,
        });
    });
    rx.await.unwrap_or_else(|_| {
        formatted_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "Responses upstream failed",
            anthropic,
        )
    })
}

/// Forward an SSE stream. If the client disconnects, keep reading the
/// upstream stream to get the final usage (ADR 0001).
fn stream_response(
    mut ctx: Ctx,
    upstream: reqwest::Response,
    status: StatusCode,
    headers: HeaderMap,
    strip_usage_chunk: bool,
    upstream_endpoint: Endpoint,
    mut translator: Option<ProxyStream>,
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
        let mut cached_body = crate::cache::StreamBody::new(
            if ctx.cache_plan.is_some() || ctx.semantic_plan.is_some() {
                ctx.st.settings().cache.max_entry_bytes
            } else {
                0
            },
        );
        // Chat/Messages translation can synthesize a stop event at EOF. Check
        // the source too, so a truncated upstream cannot become a cached hit.
        let source_check = matches!(
            ctx.provider.kind,
            ProviderKind::Openai | ProviderKind::Anthropic
        );
        let mut source_body = crate::cache::StreamBody::new(
            if source_check && (ctx.cache_plan.is_some() || ctx.semantic_plan.is_some()) {
                ctx.st.settings().cache.max_entry_bytes
            } else {
                0
            },
        );
        let mut idle_timeout = false;
        while let Some(item) = match crate::provider_limits::next_chunk(
            &mut stream,
            ctx.provider.limits.stream_idle_timeout_ms,
        )
        .await
        {
            Ok(item) => item,
            Err(_) => {
                idle_timeout = true;
                upstream_failed = true;
                ctx.trace.classify_last("stream_idle_timeout");
                metrics::counter!("tokomotive_provider_timeouts_total", "provider" => ctx.provider.name.clone(), "kind" => "stream_idle").increment(1);
                let event = if ctx.endpoint == Endpoint::Messages {
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"stream_idle_timeout\"}}\n\n"
                } else {
                    "data: {\"error\":{\"type\":\"api_error\",\"message\":\"stream_idle_timeout\"}}\n\n"
                };
                ctx.trace.response(event.as_bytes());
                if !client_gone {
                    client_gone = tx
                        .send(Ok(Bytes::from_static(event.as_bytes())))
                        .await
                        .is_err();
                }
                None
            }
        } {
            match item {
                Ok(chunk) => {
                    source_body.feed(&chunk);
                    let out = tracker.feed(&chunk);
                    let out = if let Some(t) = translator.as_mut() {
                        t.feed(&out)
                    } else {
                        out
                    };
                    if ttft.is_none() && tracker.saw_content {
                        ttft = Some(started.elapsed().as_millis() as i64);
                        ctx.trace.first_token();
                    }
                    ctx.trace.response(&out);
                    cached_body.feed(&out);
                    if !client_gone
                        && !out.is_empty()
                        && tx.send(Ok(Bytes::from(out))).await.is_err()
                    {
                        client_gone = true;
                        tracing::debug!("client disconnected; draining upstream stream");
                    }
                }
                Err(e) => {
                    let class = crate::egress::error_class(&e);
                    tracing::warn!(provider = %ctx.provider.name, class, "upstream stream failed");
                    upstream_failed = true;
                    if !client_gone {
                        let _ = tx.send(Err(std::io::Error::other(class))).await;
                    }
                    break;
                }
            }
        }
        let rest = if idle_timeout {
            Vec::new()
        } else {
            tracker.finish()
        };
        let rest = if let Some(t) = translator.as_mut().filter(|_| !idle_timeout) {
            let mut out = t.feed(&rest);
            out.extend(t.finish());
            out
        } else {
            rest
        };
        ctx.trace.response(&rest);
        cached_body.feed(&rest);
        if !client_gone && !rest.is_empty() {
            client_gone = tx.send(Ok(Bytes::from(rest))).await.is_err();
        }
        client_gone |= tx.is_closed();
        let estimated = status.is_success() && !tracker.usage_final;
        let code = if idle_timeout
            || translator.as_ref().is_some_and(ProxyStream::failed)
            || (upstream_failed && !tracker.usage_final)
        {
            502
        } else {
            status.as_u16()
        };
        let source_complete =
            !source_check || source_body.complete(upstream_endpoint.name()).is_some();
        if ctx.streamed
            && code == 200
            && !client_gone
            && !upstream_failed
            && source_complete
            && let Some(bytes) = cached_body.complete(ctx.endpoint.name())
            && let Some(write) = ctx.cache_write(
                tracker.usage.unwrap_or_default(),
                tracker.model.clone(),
                estimated,
            )
        {
            write.store(&bytes, "text/event-stream").await;
        }
        ctx.finish(Outcome {
            status: code,
            model: tracker.model.take(),
            usage: tracker.usage.unwrap_or_default(),
            reported_cost_micros: tracker.reported_cost_micros,
            estimated,
            ttft_ms: ttft,
            client_aborted: client_gone,
        });
        drop(tx);
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
