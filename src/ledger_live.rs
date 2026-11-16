//! Process-local, bounded notifications for committed Ledger Entries.
use crate::db::{LedgerFilter, LedgerQuery, LedgerRow};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AttemptFilter {
    pub error_class: String,
    pub provider_key: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct LedgerEvent {
    pub row: LedgerRow,
    pub(crate) attempts: Vec<AttemptFilter>,
}
impl LedgerEvent {
    pub(crate) fn matches(&self, q: &LedgerQuery) -> bool {
        let r = &self.row;
        match q.filter {
            LedgerFilter::Unpriced if !r.unpriced => return false,
            LedgerFilter::Estimated if !r.estimated => return false,
            LedgerFilter::Errors if r.status < 400 => return false,
            _ => {}
        }
        q.user_id.is_none_or(|v| v == r.user_id)
            && q.provider.as_ref().is_none_or(|v| v == &r.provider)
            && q.model.as_ref().is_none_or(|v| {
                Some(v) == r.model.as_ref() || Some(v) == r.requested_model.as_ref()
            })
            && q.rule.as_ref().is_none_or(|v| {
                Some(v) == r.route_rule.as_ref() || Some(v) == r.route_rule_name.as_ref()
            })
            && q.status.is_none_or(|v| v == r.status)
            && q.has_retries.is_none_or(|v| v == (r.attempts > 1))
            && q.from_ms.is_none_or(|v| r.ts_ms >= v)
            && q.to_ms.is_none_or(|v| r.ts_ms < v)
            && q.error_class
                .as_ref()
                .is_none_or(|v| self.attempts.iter().any(|a| &a.error_class == v))
            && q.provider_key
                .is_none_or(|v| self.attempts.iter().any(|a| a.provider_key == Some(v)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub cursor: String,
    pub event: Arc<LedgerEvent>,
}

#[derive(Debug)]
pub(crate) struct LedgerLive {
    epoch: u64,
    inner: Mutex<History>,
    tx: broadcast::Sender<Notification>,
    clients: Mutex<HashMap<String, usize>>,
}
#[derive(Debug, Default)]
struct History {
    sequence: u64,
    ring: VecDeque<Notification>,
}
impl Default for LedgerLive {
    fn default() -> Self {
        Self {
            epoch: rand::random(),
            inner: Mutex::new(History::default()),
            tx: broadcast::channel(1024).0,
            clients: Mutex::new(HashMap::new()),
        }
    }
}
impl LedgerLive {
    pub(crate) fn cursor(&self) -> String {
        let h = self.inner.lock().expect("Ledger history");
        format!("{}:{}", self.epoch, h.sequence)
    }
    pub(crate) fn client(self: &Arc<Self>, session: String) -> Option<Client> {
        let mut clients = self.clients.lock().expect("Live clients");
        if clients.values().sum::<usize>() >= 32 || clients.get(&session).copied().unwrap_or(0) >= 4
        {
            return None;
        }
        *clients.entry(session.clone()).or_default() += 1;
        Some(Client {
            hub: self.clone(),
            session,
        })
    }
    pub(crate) fn publish(&self, event: LedgerEvent) {
        let mut h = self.inner.lock().expect("Ledger history");
        h.sequence += 1;
        let n = Notification {
            cursor: format!("{}:{}", self.epoch, h.sequence),
            event: Arc::new(event),
        };
        if h.ring.len() == 500 {
            h.ring.pop_front();
        }
        h.ring.push_back(n.clone());
        let _ = self.tx.send(n);
    }
    /// Subscribe and snapshot under one lock: replay and live cannot leave a hole.
    pub(crate) fn subscribe(
        &self,
        cursor: Option<&str>,
    ) -> (
        broadcast::Receiver<Notification>,
        VecDeque<Notification>,
        bool,
    ) {
        let h = self.inner.lock().expect("Ledger history");
        let rx = self.tx.subscribe();
        let Some(cursor) = cursor else {
            return (rx, VecDeque::new(), false);
        };
        if cursor == format!("{}:0", self.epoch) && h.sequence <= 500 {
            return (rx, h.ring.clone(), false);
        }
        match h.ring.iter().position(|n| n.cursor == cursor) {
            Some(i) => (rx, h.ring.iter().skip(i + 1).cloned().collect(), false),
            None => (rx, VecDeque::new(), true),
        }
    }
}

pub(crate) struct Client {
    hub: Arc<LedgerLive>,
    session: String,
}
impl Drop for Client {
    fn drop(&mut self) {
        let mut clients = self.hub.clients.lock().expect("Live clients");
        if let Some(n) = clients.get_mut(&self.session) {
            *n -= 1;
            if *n == 0 {
                clients.remove(&self.session);
            }
        }
    }
}

pub(crate) async fn prepare(
    tx: &mut sqlx::SqliteConnection,
    id: i64,
    attempts: Vec<AttemptFilter>,
) -> anyhow::Result<LedgerEvent> {
    let row = sqlx::query_as(&format!("{} AND l.id=?", crate::db::LEDGER_SELECT))
        .bind(id)
        .fetch_one(tx)
        .await?;
    Ok(LedgerEvent { row, attempts })
}
