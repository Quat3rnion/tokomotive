//! Shared application state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

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
    pub kind: ProviderKind,
    pub base_url: String,
    /// The Provider Key pool. Empty when no key is configured.
    pub keys: Arc<Vec<String>>,
    next: Arc<AtomicUsize>,
}

impl Provider {
    pub fn new(name: &str, kind: ProviderKind, base_url: &str, keys: Vec<String>) -> Provider {
        Provider {
            name: name.into(),
            kind,
            base_url: base_url.into(),
            keys: Arc::new(keys),
            next: Arc::new(AtomicUsize::new(0)),
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
    user_id: i64,
    amount: i64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut r = self.st.reservations.lock().expect("reservations");
        if let Some(v) = r.get_mut(&self.user_id) {
            *v -= self.amount;
            if *v <= 0 {
                r.remove(&self.user_id);
            }
        }
    }
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: Db,
    pub http: reqwest::Client,
    router_table: RwLock<Arc<RouterTable>>,
    pub(crate) circuits: Mutex<HashMap<(String, String), crate::routing::Circuit>>,
    pub(crate) route_next: Mutex<HashMap<String, usize>>,
    pub admin_password: Option<String>,
    pub metrics_token: Option<String>,
    pub session_days: i64,
    pub reserve_output_tokens: i64,
    pub metrics: Option<PrometheusHandle>,
    reservations: Mutex<HashMap<i64, i64>>,
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
        let router_table = RouterTable::from_config(cfg);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.server.upstream_timeout_secs))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        let catalog = db::load_catalog(&db).await?;
        Ok(AppState(Arc::new(Inner {
            db,
            http,
            router_table: RwLock::new(Arc::new(router_table)),
            circuits: Mutex::new(HashMap::new()),
            route_next: Mutex::new(HashMap::new()),
            admin_password: cfg.admin_password(),
            metrics_token: cfg.metrics_token(),
            session_days: cfg.server.session_days.max(1),
            reserve_output_tokens: cfg.server.reserve_output_tokens.max(0),
            metrics,
            reservations: Mutex::new(HashMap::new()),
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

    pub fn router_table(&self) -> Arc<RouterTable> {
        self.router_table.read().expect("router table").clone()
    }

    pub fn set_router_table(&self, mut table: RouterTable) {
        let mut current = self.router_table.write().expect("router table");
        for provider in &mut table.providers {
            if let Some(old) = current.providers.iter().find(|p| p.name == provider.name) {
                provider.next = old.next.clone();
            }
        }
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
        let open = r.get(&user_id).copied().unwrap_or(0);
        if spent >= budget || spent + open + amount > budget {
            return None;
        }
        *r.entry(user_id).or_insert(0) += amount;
        Some(Reservation {
            st: self.clone(),
            user_id,
            amount,
        })
    }

    pub fn reserved(&self, user_id: i64) -> i64 {
        self.reservations
            .lock()
            .expect("reservations")
            .get(&user_id)
            .copied()
            .unwrap_or(0)
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
