//! In-memory secret resolution. Errors contain static classes, never target data.
use crate::{db::Db, egress::Egress, settings::SecretKey};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, Instant},
};

pub(crate) mod adapters;
mod bws;
pub use adapters::{browse, fields, resolve};

#[derive(Clone)]
pub struct SecretValue(String);
impl SecretValue {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Failure(pub &'static str);
pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Store {
    pub name: String,
    pub kind: String,
    pub address: String,
    pub credentials: String,
    pub options: String,
    pub allow_private: bool,
    pub is_default: bool,
    pub refresh_secs: i64,
}
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store([REDACTED])")
    }
}
impl Store {
    pub fn option(&self, name: &str) -> Option<String> {
        serde_json::from_str::<Value>(&self.options)
            .ok()?
            .get(name)?
            .as_str()
            .map(str::to_owned)
    }
    pub fn auth(&self, key: &SecretKey) -> Result<Value> {
        let raw = key
            .decrypt(&self.credentials)
            .map_err(|_| Failure("store credentials unavailable"))?;
        let mut value: Value =
            serde_json::from_str(&raw).map_err(|_| Failure("invalid store credentials"))?;
        let map = value
            .as_object_mut()
            .ok_or(Failure("invalid store credentials"))?;
        for v in map.values_mut() {
            if let Some(env) = v.as_str().and_then(|s| s.strip_prefix("env:")) {
                if env.is_empty()
                    || env.len() > 128
                    || !env.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                    })
                {
                    return Err(Failure("invalid bootstrap environment reference"));
                }
                *v = Value::String(
                    std::env::var(env)
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                        .ok_or(Failure("bootstrap environment unavailable"))?,
                );
            }
        }
        Ok(value)
    }
}
pub async fn stores(db: &Db) -> anyhow::Result<Vec<Store>> {
    Ok(
        sqlx::query_as("SELECT * FROM secret_stores ORDER BY name LIMIT 65")
            .fetch_all(db)
            .await?,
    )
}

pub fn is_secret_ref(s: &str) -> bool {
    s.split_once(':').is_some_and(|(prefix, _)| {
        matches!(
            prefix.split('@').next(),
            Some("vault" | "aws-sm" | "gcp-sm" | "bws" | "bw")
        )
    })
}
pub fn is_reference(s: &str) -> bool {
    s.starts_with("env:") || is_secret_ref(s)
}

pub struct Reference<'a> {
    pub kind: &'a str,
    pub store: Option<&'a str>,
    pub path: &'a str,
    pub field: Option<&'a str>,
}
impl<'a> Reference<'a> {
    pub fn parse(s: &'a str) -> Result<Self> {
        let (prefix, rest) = s
            .split_once(':')
            .ok_or(Failure("invalid secret reference"))?;
        let (kind, store) = prefix
            .split_once('@')
            .map_or((prefix, None), |(k, s)| (k, Some(s)));
        let (path, field) = rest
            .split_once('#')
            .map_or((rest, None), |(p, f)| (p, Some(f)));
        if !is_secret_ref(s)
            || path.is_empty()
            || s.len() > 2048
            || s.chars().any(char::is_control)
            || store == Some("")
            || field == Some("")
            || path.split('/').any(|p| matches!(p, "." | ".." | ""))
        {
            return Err(Failure("invalid secret reference"));
        }
        Ok(Self {
            kind,
            store,
            path,
            field,
        })
    }
    pub fn select<'s>(&self, stores: &'s [Store]) -> Result<&'s Store> {
        let candidates: Vec<_> = stores
            .iter()
            .filter(|s| s.kind == self.kind && self.store.is_none_or(|n| n == s.name))
            .collect();
        if candidates.len() == 1 {
            return Ok(candidates[0]);
        }
        candidates
            .into_iter()
            .find(|s| s.is_default && self.store.is_none())
            .ok_or(Failure("choose a secret store"))
    }
}

struct Entry {
    value: Option<SecretValue>,
    failed: bool,
    checked: Instant,
}
#[derive(Default)]
pub struct Manager {
    values: RwLock<HashMap<(String, String), Entry>>,
    refresh: tokio::sync::Mutex<()>,
}
impl Manager {
    pub fn get(&self, provider: &str, reference: &str) -> Option<SecretValue> {
        self.values
            .read()
            .expect("secret cache")
            .get(&(provider.into(), reference.into()))?
            .value
            .clone()
    }
    pub fn unavailable(&self, provider: &str) -> Vec<String> {
        self.values
            .read()
            .expect("secret cache")
            .iter()
            .filter(|((p, _), e)| p == provider && e.failed)
            .map(|((_, r), _)| r.clone())
            .collect()
    }
    pub async fn refresh(
        &self,
        db: &Db,
        key: &SecretKey,
        egress: &Egress,
        force: bool,
    ) -> anyhow::Result<bool> {
        let _guard = self.refresh.lock().await;
        let stores = stores(db).await?;
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT p.name,j.value FROM providers p,json_each(p.keys_json) j")
                .fetch_all(db)
                .await?;
        let references: std::collections::HashSet<_> =
            rows.into_iter().filter(|(_, v)| is_secret_ref(v)).collect();
        let removed = {
            let mut values = self.values.write().expect("secret cache");
            let n = values.len();
            values.retain(|k, _| references.contains(k));
            n != values.len()
        };
        let jobs: Vec<_> = references
            .into_iter()
            .filter(|(p, r)| {
                let values = self.values.read().expect("secret cache");
                let interval = Reference::parse(r)
                    .and_then(|r| r.select(&stores))
                    .map_or(900, |s| s.refresh_secs as u64);
                force
                    || values
                        .get(&(p.clone(), r.clone()))
                        .is_none_or(|e| e.checked.elapsed() >= Duration::from_secs(interval))
            })
            .collect();
        let changed = removed || !jobs.is_empty();
        stream::iter(jobs)
            .for_each_concurrent(4, |(provider, reference)| {
                let stores = &stores;
                async move {
                    let selection = Reference::parse(&reference).and_then(|r| r.select(stores));
                    let result = match selection {
                        Ok(store) => tokio::time::timeout(
                            Duration::from_secs(30),
                            resolve(store, key, egress, Some(&provider), &reference),
                        )
                        .await
                        .unwrap_or(Err(Failure("secret timeout"))),
                        Err(e) => Err(e),
                    };
                    let mut values = self.values.write().expect("secret cache");
                    let entry = values.entry((provider, reference)).or_insert(Entry {
                        value: None,
                        failed: true,
                        checked: Instant::now(),
                    });
                    entry.failed = result.is_err();
                    entry.checked = Instant::now();
                    if let Ok(value) = result {
                        entry.value = Some(value);
                    }
                }
            })
            .await;
        Ok(changed)
    }
}

pub fn start_background(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let _guard = st.settings_write.lock().await;
            if st.refresh_secrets(false).await.is_err() {
                tracing::warn!("secret refresh unavailable");
            }
        }
    })
}
