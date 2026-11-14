//! LLM router and proxy with optional per-model Cost, Price and Profit accounting per User.

mod affinity;
pub mod catalog;
pub mod config;
pub mod db;
pub mod metrics;
pub mod model_search;
pub mod models_dev;
pub mod oauth;
pub mod pagination;
pub mod presets;
pub mod pricing;
pub mod provider;
pub mod proxy;
pub mod ratelimit;
pub mod routing;
pub mod settings;
pub mod state;
pub mod translate;
pub mod translate_gemini;
pub mod translate_responses;
pub mod usage;
pub mod web;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get};

use crate::config::Config;
use crate::state::AppState;

/// Open the database, seed it from the configuration and build the state.
pub async fn init(
    cfg: &Config,
    metrics: Option<metrics_exporter_prometheus::PrometheusHandle>,
) -> anyhow::Result<AppState> {
    let db = db::connect(&cfg.server.database).await?;
    db::seed(&db, cfg).await?;
    AppState::new(cfg, db, metrics).await
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/admin") }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/models", get(proxy::list_models))
        .route("/v1/models/{id}", get(proxy::get_model))
        .route("/v1/{*rest}", any(proxy::handle))
        .merge(web::router())
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
            h.render(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
