use std::path::PathBuf;

use anyhow::{Context, Result};
use tokomotive::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = configuration()?;
    let metrics = tokomotive::metrics::install()?;
    let state = tokomotive::init(&cfg, Some(metrics)).await?;
    let sync = tokomotive::models_dev::start_background(state.clone());
    let batches = tokomotive::batches::start_background(state.clone());
    let retention = tokomotive::observability::start_retention(state.clone());
    let app = tokomotive::router(state.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("bind {}", cfg.server.listen))?;
    tracing::info!(listen = %cfg.server.listen, "tokomotive started");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    // Write the Ledger Entries that are still pending.
    batches.abort();
    retention.abort();
    state.flush().await;
    sync.abort();
    Ok(())
}

fn configuration() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    let mut path = std::env::var("TOKOMOTIVE_CONFIG").ok().map(PathBuf::from);
    let mut listen = std::env::var("TOKOMOTIVE_LISTEN").ok();
    let mut database = std::env::var("TOKOMOTIVE_DATABASE").ok();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => path = Some(args.next().context("--config needs a path")?.into()),
            "--listen" => listen = Some(args.next().context("--listen needs an address")?),
            "--database" => database = Some(args.next().context("--database needs a SQLite URL")?),
            "-h" | "--help" => {
                println!(
                    "usage: tokomotive [--config PATH] [--listen ADDRESS] [--database SQLITE_URL]\n\nThe optional config file seeds the database once. Use /setup, then the dashboard.\nEnvironment: TOKOMOTIVE_CONFIG, TOKOMOTIVE_LISTEN, TOKOMOTIVE_DATABASE"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let mut cfg = match path {
        Some(p) => Config::load(&p)?,
        None => toml::from_str("")?,
    };
    // Binding and storage are process options, not dashboard settings.
    cfg.server.listen = listen.unwrap_or_else(|| tokomotive::config::Server::default().listen);
    cfg.server.database =
        database.unwrap_or_else(|| tokomotive::config::Server::default().database);
    Ok(cfg)
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
