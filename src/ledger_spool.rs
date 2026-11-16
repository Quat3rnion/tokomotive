//! Durable fallback for Ledger Entries. Database keys make crash replay safe.
use crate::db::{self, Db, NewEntry};
use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Spool {
    path: Option<PathBuf>,
    limit: AtomicU64,
    pub(crate) pending: Arc<tokio::sync::RwLock<HashMap<String, PendingCharge>>>,
}

#[derive(Debug)]
pub(crate) struct PendingCharge {
    user_id: i64,
    key_id: i64,
    team_id: Option<i64>,
    ts_ms: i64,
    price_micros: i64,
}
impl PendingCharge {
    pub(crate) fn spend(&self, subject: crate::budgets::Subject, start: i64, end: i64) -> i64 {
        use crate::budgets::Subject;
        let matches = match subject {
            Subject::User(id) => self.user_id == id,
            Subject::Key(id) => self.key_id == id,
            Subject::Team(id) => self.team_id == Some(id),
        };
        if matches && self.ts_ms >= start && self.ts_ms < end {
            self.price_micros
        } else {
            0
        }
    }
}
impl From<&NewEntry> for PendingCharge {
    fn from(entry: &NewEntry) -> Self {
        Self {
            user_id: entry.user_id,
            key_id: entry.key_id,
            team_id: entry.team_id,
            ts_ms: entry.ts_ms,
            price_micros: entry.quote.price_micros,
        }
    }
}
impl Spool {
    pub(crate) fn new(url: &str) -> Result<Self> {
        let path = if url.contains(":memory:") {
            None
        } else {
            Some(crate::settings::database_path(url)?.with_file_name("ledger-spool.jsonl"))
        };
        let mut pending = HashMap::new();
        if let Some(path) = &path {
            for segment in [path.clone(), path.with_extension("replay.jsonl")] {
                if !segment.exists() {
                    continue;
                }
                let mut validate = || -> Result<()> {
                    let mut reader = BufReader::new(File::open(&segment)?);
                    loop {
                        let mut line = Vec::new();
                        if reader.read_until(b'\n', &mut line)? == 0 {
                            return Ok(());
                        }
                        anyhow::ensure!(line.last() == Some(&b'\n'), "incomplete record");
                        let entry: PendingEntry = serde_json::from_slice(&line)?;
                        pending.insert(entry.write_key, PendingCharge::from(&entry.entry));
                    }
                };
                validate().with_context(|| format!(
                    "invalid Ledger spool {}; stop the service and follow docs/configuration.md#ledger-spool-recovery",
                    segment.display()
                ))?;
            }
        }
        Ok(Self {
            path,
            limit: AtomicU64::new(DEFAULT_MAX_BYTES),
            pending: Arc::new(tokio::sync::RwLock::new(pending)),
        })
    }
    pub(crate) fn set_limit(&self, bytes: u64) {
        self.limit
            .store(bytes.clamp(1, 1024 * 1024 * 1024), Ordering::Relaxed);
    }
    async fn append(self: &Arc<Self>, entry: &PendingEntry) -> Result<()> {
        let path = self
            .path
            .clone()
            .context("Ledger spool requires a file database")?;
        let limit = self.limit.load(Ordering::Relaxed);
        let mut bytes = serde_json::to_vec(entry)?;
        bytes.push(b'\n');
        let write_key = entry.write_key.clone();
        let charge = PendingCharge::from(&entry.entry);
        // Keep the durable handoff and its Budget charge together, even if the
        // caller is cancelled while the blocking append completes.
        let mut pending = self.pending.clone().write_owned().await;
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatcher, || {
                let _lock = lock(&path.with_extension("lock"), false)?.context("spool lock")?;
                let used = spool_bytes(&path)?;
                if used.saturating_add(bytes.len() as u64) > limit {
                    metrics::counter!("tokomotive_ledger_spool_full_total").increment(1);
                    tracing::error!(
                        used_bytes = used,
                        limit_bytes = limit,
                        "Ledger spool is full; keep the Reservation and retry"
                    );
                    anyhow::bail!("Ledger spool is full ({used}/{limit} bytes)");
                }
                let mut file = private_file(&path, true)?;
                let length = file.metadata()?.len();
                if length > 0 {
                    file.seek(SeekFrom::End(-1))?;
                    let mut tail = [0];
                    file.read_exact(&mut tail)?;
                    anyhow::ensure!(
                        tail[0] == b'\n',
                        "incomplete Ledger spool tail; repair the spool before appending"
                    );
                }
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    // Do not leave half a record ahead of the next append.
                    file.set_len(length)?;
                    file.sync_all()?;
                    return Err(error.into());
                }
                sync_parent(&path)?;
                pending.insert(write_key, charge);
                metrics::gauge!("tokomotive_ledger_spool_bytes")
                    .set((used + bytes.len() as u64) as f64);
                metrics::counter!("tokomotive_ledger_spooled_total").increment(1);
                Ok(())
            })
        })
        .await?
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PendingEntry {
    write_key: String,
    entry: NewEntry,
    #[serde(default)]
    attempts: Vec<crate::ledger_live::AttemptFilter>,
}
impl PendingEntry {
    pub(crate) fn new(entry: NewEntry) -> Self {
        let mut key = [0u8; 24];
        rand::rng().fill_bytes(&mut key);
        Self {
            write_key: hex::encode(key),
            entry,
            attempts: Vec::new(),
        }
    }
    pub(crate) fn with_attempts(mut self, attempts: &[crate::observability::Attempt]) -> Self {
        self.attempts = attempts
            .iter()
            .map(|a| crate::ledger_live::AttemptFilter {
                error_class: a.error_class.clone(),
                provider_key: a.provider_key,
            })
            .collect();
        self
    }
    pub(crate) async fn write_in(
        &self,
        tx: &mut sqlx::SqliteConnection,
    ) -> Result<(i64, Option<crate::ledger_live::LedgerEvent>)> {
        if let Some(id) = sqlx::query_scalar("SELECT id FROM ledger WHERE write_key=?")
            .bind(&self.write_key)
            .fetch_optional(&mut *tx)
            .await?
        {
            return Ok((id, None));
        }
        let id = db::insert_entry_in(tx, &self.entry).await?;
        sqlx::query("UPDATE ledger SET write_key=? WHERE id=?")
            .bind(&self.write_key)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let event = crate::ledger_live::prepare(tx, id, self.attempts.clone()).await?;
        Ok((id, Some(event)))
    }
    async fn write(&self, db: &Db) -> Result<i64> {
        let mut tx = db::begin_write(db, "ledger").await?;
        let (id, event) = self.write_in(&mut tx).await?;
        tx.commit().await?;
        if let Some(event) = event {
            db.ledger_live.publish(event);
        }
        Ok(id)
    }
    pub(crate) async fn spool(&self, db: &Db) -> Result<()> {
        db.spool.append(self).await
    }
    pub(crate) async fn persist(&self, db: &Db) -> Result<Option<i64>> {
        match self.write(db).await {
            Ok(id) => Ok(Some(id)),
            Err(error) => {
                tracing::error!(error=%error, write_key=%self.write_key, "Ledger write failed; use the durable spool");
                self.spool(db).await?;
                Ok(None)
            }
        }
    }
    /// Wait for writer admission before taking the Budget lock. Otherwise one
    /// stuck Ledger writer also prevents every new Budget check from running.
    pub(crate) async fn persist_reserved(
        &self,
        st: &crate::state::AppState,
        mut reservation: Option<crate::state::Reservation>,
    ) -> Option<i64> {
        loop {
            let written = match db::begin_write(&st.db, "ledger").await {
                Ok(mut tx) => {
                    let _guard = st.budget_check.lock().await;
                    let result = async {
                        let (id, event) = self.write_in(&mut tx).await?;
                        tx.commit().await?;
                        if let Some(event) = event {
                            st.ledger_live.publish(event);
                        }
                        Ok::<_, anyhow::Error>(id)
                    }
                    .await;
                    if let Ok(id) = result {
                        drop(reservation.take());
                        return Some(id);
                    }
                    result
                }
                Err(e) => Err(e),
            };
            if let Err(error) = written {
                tracing::error!(error=%error, write_key=%self.write_key, "Ledger write failed; use the durable spool");
            }
            match self.spool(&st.db).await {
                Ok(()) => {
                    let _guard = st.budget_check.lock().await;
                    drop(reservation.take());
                    return None;
                }
                Err(error) => {
                    tracing::error!(error=%error, "Ledger is not durable; retain its Reservation and retry")
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

fn private_file(path: &Path, append: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        .append(append)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn lock(path: &Path, nonblocking: bool) -> Result<Option<File>> {
    let file = private_file(path, false)?;
    if nonblocking {
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    } else {
        file.lock()?;
    }
    Ok(Some(file))
}
fn sync_parent(path: &Path) -> Result<()> {
    File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}
fn size(path: &Path) -> Result<u64> {
    match path.metadata() {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}
fn spool_bytes(path: &Path) -> Result<u64> {
    Ok(size(path)?.saturating_add(size(&path.with_extension("replay.jsonl"))?))
}

/// Replay one rotated segment. Keep it until every entry commits. A crash
/// between commit and removal replays the same keys, not a second Ledger row.
pub async fn replay(db: &Db) -> Result<u64> {
    replay_segment(db, false).await
}

/// No request may enter until every startup segment is valid and committed.
pub(crate) async fn drain(db: &Db) -> Result<()> {
    let Some(path) = &db.spool.path else {
        return Ok(());
    };
    loop {
        replay_segment(db, true).await?;
        if spool_bytes(path)? == 0 && !path.with_extension("replay.jsonl").exists() {
            return Ok(());
        }
    }
}

async fn replay_segment(db: &Db, startup: bool) -> Result<u64> {
    let Some(path) = db.spool.path.clone() else {
        return Ok(0);
    };
    let prepare_path = path.clone();
    let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
        let Some(replay_lock) = lock(&prepare_path.with_extension("replay.lock"), true)? else {
            anyhow::ensure!(
                !startup,
                "Ledger spool replay is already running; stop the other process before startup"
            );
            return Ok(None);
        };
        let _lock = lock(&prepare_path.with_extension("lock"), false)?;
        let replay = prepare_path.with_extension("replay.jsonl");
        if !replay.exists() {
            if size(&prepare_path)? == 0 {
                return Ok(None);
            }
            std::fs::rename(&prepare_path, &replay)?;
            sync_parent(&prepare_path)?;
        }
        let reader = BufReader::new(File::open(&replay)?);
        Ok(Some((replay_lock, reader)))
    })
    .await??;
    let Some((_replay_lock, mut reader)) = prepared else {
        return Ok(0);
    };
    let mut count = 0;
    loop {
        let (next, entries) = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut entries = Vec::new();
            for _ in 0..100 {
                let mut line = String::new();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                anyhow::ensure!(
                    line.ends_with('\n'),
                    "incomplete Ledger spool record; restore the spool from backup"
                );
                entries.push(
                    serde_json::from_str::<PendingEntry>(&line)
                        .context("invalid Ledger spool record")?,
                );
            }
            Ok((reader, entries))
        })
        .await??;
        reader = next;
        if entries.is_empty() {
            break;
        }
        for entry in entries {
            // Never wait for SQLite with the pending-charge lock held. Budget
            // readers can proceed while another writer blocks replay.
            let mut tx = db::begin_write(db, "ledger_spool").await?;
            let mut pending = db.spool.pending.write().await;
            let (_, event) = entry.write_in(&mut tx).await?;
            tx.commit().await?;
            if let Some(event) = event {
                db.ledger_live.publish(event);
            }
            pending.remove(&entry.write_key);
            count += 1;
            metrics::counter!("tokomotive_ledger_spool_replayed_total").increment(1);
        }
    }
    tokio::task::spawn_blocking(move || -> Result<()> {
        let _lock = lock(&path.with_extension("lock"), false)?;
        std::fs::remove_file(path.with_extension("replay.jsonl"))?;
        sync_parent(&path)?;
        metrics::gauge!("tokomotive_ledger_spool_bytes").set(spool_bytes(&path)? as f64);
        Ok(())
    })
    .await??;
    Ok(count)
}

pub fn start_replay(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(e) = replay(&st.db).await {
                tracing::error!(error=%e, "Ledger spool replay failed; keep the segment for the next tick");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_spool_retains_reservation_without_holding_budget_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: crate::config::Config = toml::from_str(&format!(
            "[server]\ndatabase='sqlite://{}/db'\n[[users]]\nname='alice'\nkeys=['key']",
            dir.path().display()
        ))
        .unwrap();
        let st = crate::init(&cfg, None).await.unwrap();
        st.db.configure_writer(1, 1);
        let entry = NewEntry {
            route_rule: None,
            cache_hit: false,
            ts_ms: db::now_ms(),
            user_id: 1,
            key_id: 1,
            team_id: None,
            provider: "test".into(),
            subscription: false,
            endpoint: "chat/completions".into(),
            requested_model: Some("model".into()),
            model: None,
            status: 200,
            streamed: false,
            usage: Default::default(),
            reported_cost_micros: None,
            quote: Default::default(),
            estimated: false,
            latency_ms: 0,
            ttft_ms: None,
            client_aborted: false,
            provider_key: None,
            attempts: 1,
            session_affinity: None,
        };
        let pending = PendingEntry::new(entry);
        let reservation = st.reserve(1, 100, 0, 24).unwrap();
        let writer = st.db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let worker_st = st.clone();
        let worker = tokio::spawn(async move {
            pending
                .persist_reserved(&worker_st, Some(reservation))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        assert!(!worker.is_finished());
        assert_eq!(st.reserved(1), 24, "a full spool is not a durable handoff");
        let _budget_guard = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            st.budget_check.lock(),
        )
        .await
        .expect("writer contention must not hold the Budget lock");
        drop(_budget_guard);
        writer.rollback().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), worker)
                .await
                .unwrap()
                .unwrap()
                .is_some()
        );
        assert_eq!(st.reserved(1), 0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger")
            .fetch_one(&st.db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
