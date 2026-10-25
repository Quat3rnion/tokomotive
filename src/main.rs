use std::path::PathBuf;

use anyhow::{Context, Result};
use llm_accounting::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = config_path()?;
    let mut cfg = Config::load(&path)?;
    if let Ok(v) = std::env::var("LLM_ACCOUNTING_LISTEN") {
        cfg.server.listen = v;
    }
    if let Ok(v) = std::env::var("LLM_ACCOUNTING_DATABASE") {
        cfg.server.database = v;
    }
    if cfg.admin_password().is_none() {
        tracing::warn!("no admin password is set; the dashboard is off");
    }
    let metrics = llm_accounting::metrics::install()?;
    let state = llm_accounting::init(&cfg, Some(metrics)).await?;
    let app = llm_accounting::router(state.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("bind {}", cfg.server.listen))?;
    tracing::info!(listen = %cfg.server.listen, config = %path.display(), "llm-accounting started");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    // Write the Ledger Entries that are still pending.
    state.flush().await;
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut path = std::env::var("LLM_ACCOUNTING_CONFIG")
        .ok()
        .map(PathBuf::from);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => path = Some(args.next().context("--config needs a path")?.into()),
            "-h" | "--help" => {
                println!(
                    "usage: llm-accounting [--config PATH]\n\nDefault config: ./config.toml (or LLM_ACCOUNTING_CONFIG)"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(path.unwrap_or_else(|| PathBuf::from("config.toml")))
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
