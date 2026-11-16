//! LLM router and proxy with optional per-model Cost, Price and Profit accounting per User.

pub mod adaptive;
pub mod admin;
mod affinity;
pub mod alerts;
pub mod batches;
pub mod budgets;
pub mod cache;
pub mod catalog;
pub mod config;
pub mod db;
pub mod egress;
pub mod guardrails;
pub mod inbound;
mod ledger_live;
mod ledger_spool;
pub mod mcp;
pub mod metrics;
pub mod model_search;
pub mod models_dev;
pub mod oauth;
pub mod observability;
pub mod pagination;
pub mod presets;
pub mod pricing;
pub mod provider;
pub mod provider_limits;
pub mod provider_types;
pub mod proxy;
pub mod quota;
pub mod ratelimit;
pub mod resolve;
pub mod route_rules;
pub mod routing;
pub mod semantic_cache;
pub mod settings;
pub mod state;
pub mod translate;
pub mod translate_gemini;
pub mod translate_responses;
pub mod usage;
pub mod vertex;
pub mod web;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};

use crate::config::Config;
use crate::state::AppState;

/// Open the database, seed it from the configuration and build the state.
pub async fn init(
    cfg: &Config,
    metrics: Option<metrics_exporter_prometheus::PrometheusHandle>,
) -> anyhow::Result<AppState> {
    let db = db::connect(&cfg.server.database).await?;
    db::seed(&db, cfg).await?;
    let state = AppState::new(cfg, db, metrics).await?;
    // Fail closed at startup if a previous process left uncommitted accounting.
    ledger_spool::drain(&state.db).await?;
    Ok(state)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/admin") }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/models", get(inbound::list_models))
        .route(
            "/v1/models/{*rest}",
            get(inbound::get_model).post(inbound::handle),
        )
        .route("/v1beta/models", get(inbound::list_models))
        .route(
            "/v1beta/models/{*rest}",
            get(inbound::get_model).post(inbound::handle),
        )
        .route("/model/{*rest}", any(inbound::handle))
        .route("/v1/messages/count_tokens", post(proxy::handle))
        .route("/v1/rerank", post(proxy::handle))
        .route("/v1/audio/transcriptions", post(proxy::handle))
        .route("/v1/audio/translations", post(proxy::handle))
        .route("/v1/audio/speech", post(proxy::handle))
        .route("/v1/files", any(batches::handle))
        .route("/v1/files/{*rest}", any(batches::handle))
        .route("/v1/batches", any(batches::handle))
        .route("/v1/batches/{*rest}", any(batches::handle))
        .route("/v1/{*rest}", any(proxy::handle))
        .merge(web::router(state.clone()))
        .merge(mcp::router())
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

async fn metrics_endpoint(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if let Some(token) = &st.settings().metrics_token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| web::ct_eq(g.trim(), token)) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
            )
                .into_response();
        }
    }
    match &st.metrics {
        Some(h) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            format!(
                "{}{}",
                h.render(),
                crate::quota::metrics(&st.db).await.unwrap_or_default()
            ),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
pub mod eventstream;
pub mod sigv4;
pub mod translate_bedrock;
pub mod translate_cohere;
