//! Shared application state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::Notify;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::db::{self, Db};
use crate::provider::ProviderKind;
use crate::routing::RouterTable;

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub key_strategy: crate::config::KeyStrategy,
    pub kind: ProviderKind,
    pub auth: crate::provider::ProviderAuth,
    pub include_usage: bool,
    pub key_header: crate::provider::KeyHeader,
    pub base_url: String,
    pub project_id: String,
    pub region: String,
    /// The Provider Key pool. Empty when no key is configured.
    pub keys: Arc<Vec<String>>,
    next: Arc<AtomicUsize>,
    /// Client policy from the same runtime snapshot as these credentials.
    pub(crate) egress: Option<crate::egress::Egress>,
}

impl Provider {
    pub fn new(name: &str, kind: ProviderKind, base_url: &str, keys: Vec<String>) -> Provider {
        Provider {
            name: name.into(),
            key_strategy: crate::config::KeyStrategy::default(),
            kind,
            auth: crate::provider::ProviderAuth::ApiKey,
            include_usage: true,
            key_header: crate::provider::KeyHeader::for_kind(kind),
            base_url: base_url.into(),
            project_id: String::new(),
            region: crate::config::default_region(),
            keys: Arc::new(keys),
            next: Arc::new(AtomicUsize::new(0)),
            egress: None,
        }
    }

    /// The order of key indexes to try for one request: round-robin start,
    /// then the other keys in turn.
    pub fn key_order(&self) -> Vec<usize> {
        let n = self.keys.len();
        if n == 0 {
            return vec![];
        }
        let start = self.next.fetch_add(1, Ordering::Relaxed) % n;
        (0..n).map(|i| (start + i) % n).collect()
    }
}

/// A Budget reservation. It is released when dropped.
pub struct Reservation {
    st: AppState,
    subjects: Vec<(crate::budgets::Subject, i64)>,
    amount: i64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut r = self.st.reservations.lock().expect("reservations");
        for subject in &self.subjects {
            if let Some(v) = r.get_mut(subject) {
                *v -= self.amount;
                if *v <= 0 {
                    r.remove(subject);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub mcp: crate::mcp::Sessions,
    pub db: Db,
    pub limiter: crate::ratelimit::RateLimiter,
    #[cfg(test)]
    test_http: RwLock<Option<reqwest::Client>>,
    pub egress: crate::egress::Egress,
    settings: RwLock<Arc<crate::settings::Settings>>,
    pub secret_key: crate::settings::SecretKey,
    pub oauth: crate::oauth::OAuthManager,
    pub settings_write: tokio::sync::Mutex<()>,
    pub(crate) dashboard: crate::web::accounts::DashboardState,
    /// Serialize spend reads with Ledger commits and Reservation release.
    pub(crate) budget_check: tokio::sync::Mutex<()>,
    router_table: RwLock<Arc<RouterTable>>,
    pub(crate) circuits: Mutex<HashMap<(String, String), crate::routing::Circuit>>,
    pub(crate) route_next: Mutex<HashMap<String, usize>>,
    pub(crate) affinity: Mutex<crate::affinity::AffinityTable>,
    pub metrics: Option<PrometheusHandle>,
    pub observability: crate::observability::Observer,
    reservations: Mutex<HashMap<(crate::budgets::Subject, i64), i64>>,
    catalog: RwLock<Arc<Catalog>>,
    /// The model picker sources. Register more with `model_sources.register`.
    pub model_sources: crate::model_search::Registry,
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
        Self::new_with_oauth_endpoints(cfg, db, metrics, crate::oauth::OAuthEndpoints::default())
            .await
    }

    pub async fn new_with_oauth_endpoints(
        cfg: &Config,
        db: Db,
        metrics: Option<PrometheusHandle>,
        endpoints: crate::oauth::OAuthEndpoints,
    ) -> Result<AppState> {
        let secret_key = crate::settings::SecretKey::load(&cfg.server.database)?;
        crate::settings::seed(&db, cfg, &secret_key).await?;
        crate::models_dev::seed(&db).await?;
        let (runtime, settings) = crate::settings::load(&db, &secret_key).await?;
        let mut router_table = RouterTable::from_config(&runtime);
        let egress = crate::egress::Egress::new(
            crate::egress::Policy::load(&db, &secret_key, settings.upstream_timeout_secs).await?,
        );
        for provider in &mut router_table.providers {
            provider.egress = Some(egress.snapshot());
        }
        let catalog = db::load_catalog(&db).await?;
        let oauth = crate::oauth::OAuthManager::new(db.clone(), secret_key.clone(), endpoints)?
            .with_egress(egress.clone());
        oauth.check_storage().await?;
        let observability = crate::observability::Observer::new(
            crate::observability::load(&db, &secret_key).await?,
        )?;
        Ok(AppState(Arc::new(Inner {
            mcp: crate::mcp::Sessions::default(),
            db,
            limiter: crate::ratelimit::RateLimiter::new(),
            #[cfg(test)]
            test_http: RwLock::new(None),
            egress,
            settings: RwLock::new(Arc::new(settings)),
            secret_key,
            oauth,
            settings_write: tokio::sync::Mutex::new(()),
            dashboard: crate::web::accounts::DashboardState::new()?,
            budget_check: tokio::sync::Mutex::new(()),
            router_table: RwLock::new(Arc::new(router_table)),
            circuits: Mutex::new(HashMap::new()),
            route_next: Mutex::new(HashMap::new()),
            affinity: Mutex::new(crate::affinity::AffinityTable::default()),
            metrics,
            observability,
            reservations: Mutex::new(HashMap::new()),
            catalog: RwLock::new(Arc::new(catalog)),
            model_sources: crate::model_search::Registry::with_builtin(),
            pending: AtomicUsize::new(0),
            idle: Notify::new(),
        })))
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().expect("catalog lock").clone()
    }

    pub fn settings(&self) -> Arc<crate::settings::Settings> {
        self.settings.read().expect("settings lock").clone()
    }

    pub fn http(&self) -> reqwest::Client {
        self.egress
            .client(None, crate::egress::Purpose::Upstream)
            .expect("global client")
    }
    pub fn provider_http(&self, name: &str) -> Result<reqwest::Client> {
        // Preserve the trusted TLS test seam used by Bedrock tests.
        #[cfg(test)]
        if let Some(http) = self.test_http.read().expect("test http").as_ref() {
            return Ok(http.clone());
        }
        self.egress
            .client(Some(name), crate::egress::Purpose::Upstream)
    }
    pub(crate) async fn snapshot_http(&self, provider: &Provider) -> Result<reqwest::Client> {
        #[cfg(test)]
        if let Some(http) = self.test_http.read().expect("test http").as_ref() {
            return Ok(http.clone());
        }
        let egress = provider
            .egress
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Provider egress policy unavailable"))?;
        egress.validate(&self.db, &provider.name).await?;
        egress.client(Some(&provider.name), crate::egress::Purpose::Upstream)
    }
    #[cfg(test)]
    pub(crate) fn set_test_http(&self, http: reqwest::Client) {
        *self.test_http.write().expect("test http") = Some(http);
    }

    pub async fn reload_settings(&self) -> Result<()> {
        let (cfg, settings) = crate::settings::load(&self.db, &self.secret_key).await?;
        let policy =
            crate::egress::Policy::load(&self.db, &self.secret_key, settings.upstream_timeout_secs)
                .await?;
        self.egress.replace(policy);
        *self.settings.write().expect("settings lock") = Arc::new(settings);
        self.set_router_table(RouterTable::from_config(&cfg));
        self.reload_catalog().await
    }

    pub async fn reload_catalog(&self) -> Result<()> {
        let c = db::load_catalog(&self.db).await?;
        *self.catalog.write().expect("catalog lock") = Arc::new(c);
        Ok(())
    }

    pub fn router_table(&self) -> Arc<RouterTable> {
        self.router_table.read().expect("router table").clone()
    }

    pub fn set_router_table(&self, mut table: RouterTable) {
        let mut current = self.router_table.write().expect("router table");
        for provider in &mut table.providers {
            provider.egress = Some(self.egress.snapshot());
            if let Some(old) = current.providers.iter().find(|p| p.name == provider.name) {
                provider.next = old.next.clone();
            }
        }
        self.affinity
            .lock()
            .expect("session affinity")
            .configure(&table);
        *current = Arc::new(table);
    }

    /// Reserve `amount` of a User's Budget if `spent` plus all open
    /// reservations plus `amount` stay within `budget`.
    pub fn reserve(
        &self,
        user_id: i64,
        budget: i64,
        spent: i64,
        amount: i64,
    ) -> Option<Reservation> {
        let mut r = self.reservations.lock().expect("reservations");
        let subject = (crate::budgets::Subject::User(user_id), 0);
        let open = r.get(&subject).copied().unwrap_or(0);
        if spent >= budget || spent + open + amount > budget {
            return None;
        }
        *r.entry(subject).or_insert(0) += amount;
        Some(Reservation {
            st: self.clone(),
            subjects: vec![subject],
            amount,
        })
    }

    pub fn reserved(&self, user_id: i64) -> i64 {
        self.reservations
            .lock()
            .expect("reservations")
            .iter()
            .filter(|((s, _), _)| *s == crate::budgets::Subject::User(user_id))
            .map(|(_, amount)| amount)
            .sum()
    }

    pub fn reserve_budgets(
        &self,
        budgets: &[crate::budgets::Budget],
        amount: i64,
    ) -> Option<Reservation> {
        let mut r = self.reservations.lock().expect("reservations");
        let amount = amount.max(0);
        for b in budgets {
            let open = r.get(&(b.subject, b.start)).copied().unwrap_or(0);
            if let Some(limit) = b.limit
                && (b.spent >= limit || b.spent.saturating_add(open).saturating_add(amount) > limit)
            {
                return None;
            }
        }
        let subjects = budgets
            .iter()
            .map(|b| (b.subject, b.start))
            .collect::<Vec<_>>();
        for subject in &subjects {
            *r.entry(*subject).or_default() += amount;
        }
        Some(Reservation {
            st: self.clone(),
            subjects,
            amount,
        })
    }

    /// Spawn a background write. `flush` waits for all of them.
    pub fn spawn_tracked<F: std::future::Future<Output = ()> + Send + 'static>(&self, fut: F) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        struct Pending(AppState);
        impl Drop for Pending {
            fn drop(&mut self) {
                if self.0.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
                    self.0.idle.notify_waiters();
                }
            }
        }
        let pending = Pending(self.clone());
        tokio::spawn(async move {
            let _pending = pending;
            fut.await;
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
