//! LLM API proxy that accounts per-model Cost, Price and Profit per User.

pub mod catalog;
pub mod config;
pub mod db;
pub mod metrics;
pub mod pricing;
pub mod provider;
pub mod proxy;
pub mod state;
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
        .route("/v1/{*rest}", any(proxy::handle))
        .merge(web::router())
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

async fn metrics_endpoint(State(st): State<AppState>) -> Response {
    match &st.metrics {
        Some(h) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            h.render(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
