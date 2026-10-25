//! Shared application state.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::Notify;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::db::{self, Db};
use crate::provider::{Endpoint, ProviderKind};

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub key: String,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: Db,
    pub http: reqwest::Client,
    pub providers: Vec<Provider>,
    pub admin_password: Option<String>,
    pub sessions: Mutex<HashSet<String>>,
    pub metrics: Option<PrometheusHandle>,
    catalog: RwLock<Arc<Catalog>>,
    pending: AtomicUsize,
    idle: Notify,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub async fn new(cfg: &Config, db: Db, metrics: Option<PrometheusHandle>) -> Result<AppState> {
        let providers = cfg
            .providers
            .iter()
            .map(|p| {
                let key = p.key().unwrap_or_else(|| {
                    tracing::warn!(provider = %p.name, "no API key is set for this provider");
                    String::new()
                });
                Provider {
                    name: p.name.clone(),
                    kind: p.kind,
                    base_url: p.base_url.clone(),
                    key,
                }
            })
            .collect();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.server.upstream_timeout_secs))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        let catalog = db::load_catalog(&db).await?;
        Ok(AppState(Arc::new(Inner {
            db,
            http,
            providers,
            admin_password: cfg.admin_password(),
            sessions: Mutex::new(HashSet::new()),
            metrics,
            catalog: RwLock::new(Arc::new(catalog)),
            pending: AtomicUsize::new(0),
            idle: Notify::new(),
        })))
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().expect("catalog lock").clone()
    }

    pub async fn reload_catalog(&self) -> Result<()> {
        let c = db::load_catalog(&self.db).await?;
        *self.catalog.write().expect("catalog lock") = Arc::new(c);
        Ok(())
    }

    /// Choose the Provider: the Model's Provider, else the one that speaks the
    /// endpoint, else Anthropic when the client sent `anthropic-version`, else
    /// the first OpenAI-compatible Provider.
    pub fn pick_provider(
        &self,
        catalog: &Catalog,
        model: Option<&str>,
        endpoint: Endpoint,
        anthropic_hint: bool,
    ) -> Option<Provider> {
        if let Some(name) = model.and_then(|m| catalog.provider_for(m))
            && let Some(p) = self.providers.iter().find(|p| p.name == name)
        {
            return Some(p.clone());
        }
        let kind = endpoint.native_kind().unwrap_or(if anthropic_hint {
            ProviderKind::Anthropic
        } else {
            ProviderKind::Openai
        });
        self.providers
            .iter()
            .find(|p| p.kind == kind)
            .or(self.providers.first())
            .cloned()
    }

    /// Spawn a background write. `flush` waits for all of them.
    pub fn spawn_tracked<F: std::future::Future<Output = ()> + Send + 'static>(&self, fut: F) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        tokio::spawn(async move {
            fut.await;
            if me.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
                me.idle.notify_waiters();
            }
        });
    }

    pub async fn flush(&self) {
        loop {
            let notified = self.idle.notified();
            if self.pending.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}
