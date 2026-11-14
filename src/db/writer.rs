//! Per-pool writer admission. Waiters do not consume pool connections.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::Result;
use futures_util::{future::BoxFuture, stream::BoxStream};
use sqlx::{
    Either, Execute, Executor, Sqlite, SqlitePool,
    sqlite::{SqliteQueryResult, SqliteRow, SqliteStatement, SqliteTypeInfo},
};

#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
    writer: Arc<tokio::sync::Semaphore>,
    timeout_ms: Arc<AtomicU64>,
    pub(crate) spool: Arc<crate::ledger_spool::Spool>,
    #[cfg(debug_assertions)]
    owner: Arc<std::sync::Mutex<Option<tokio::task::Id>>>,
}

impl std::ops::Deref for Db {
    type Target = SqlitePool;
    fn deref(&self) -> &Self::Target {
        &self.pool
    }
}

impl Db {
    pub(crate) fn new(pool: SqlitePool, spool: crate::ledger_spool::Spool) -> Self {
        Self {
            pool,
            writer: Arc::new(tokio::sync::Semaphore::new(1)),
            timeout_ms: Arc::new(AtomicU64::new(120_000)),
            spool: Arc::new(spool),
            #[cfg(debug_assertions)]
            owner: Arc::new(std::sync::Mutex::new(None)),
        }
    }
    pub(crate) fn configure_writer(&self, seconds: u64, spool_bytes: u64) {
        self.timeout_ms
            .store(seconds.clamp(1, 3600) * 1000, Ordering::Relaxed);
        self.spool.set_limit(spool_bytes);
    }
    fn check_pool_access(&self) {
        #[cfg(debug_assertions)]
        if let Some(id) = tokio::task::try_id() {
            let owner = *self.owner.lock().expect("writer owner");
            assert_ne!(
                owner,
                Some(id),
                "pool access while this task holds a write transaction; use the transaction connection"
            );
        }
    }
    pub async fn acquire(&self) -> Result<sqlx::pool::PoolConnection<Sqlite>, sqlx::Error> {
        self.check_pool_access();
        self.pool.acquire().await
    }
    pub async fn begin(&self) -> Result<sqlx::Transaction<'static, Sqlite>, sqlx::Error> {
        self.check_pool_access();
        self.pool.begin().await
    }
}

// Keep the existing query API. The debug check catches accidental re-entry
// through the pool from a writer task instead of silently deadlocking it.
impl<'c> Executor<'c> for &'c Db {
    type Database = Sqlite;
    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<SqliteQueryResult, SqliteRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        self.check_pool_access();
        self.pool.fetch_many(query)
    }
    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<SqliteRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        self.check_pool_access();
        self.pool.fetch_optional(query)
    }
    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [SqliteTypeInfo],
    ) -> BoxFuture<'e, Result<SqliteStatement<'q>, sqlx::Error>>
    where
        'c: 'e,
    {
        self.check_pool_access();
        self.pool.prepare_with(sql, parameters)
    }
    fn describe<'e, 'q: 'e>(
        self,
        sql: &'q str,
    ) -> BoxFuture<'e, Result<sqlx::Describe<Sqlite>, sqlx::Error>>
    where
        'c: 'e,
    {
        self.check_pool_access();
        self.pool.describe(sql)
    }
}

pub(crate) struct WriteTransaction {
    tx: Option<sqlx::Transaction<'static, Sqlite>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    #[cfg(debug_assertions)]
    owner: Arc<std::sync::Mutex<Option<tokio::task::Id>>>,
}
impl std::ops::Deref for WriteTransaction {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        self.tx.as_ref().expect("open writer")
    }
}
impl std::ops::DerefMut for WriteTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx.as_mut().expect("open writer")
    }
}
impl WriteTransaction {
    pub(crate) async fn commit(mut self) -> Result<()> {
        self.tx.take().expect("open writer").commit().await?;
        Ok(())
    }
}
impl Drop for WriteTransaction {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            *self.owner.lock().expect("writer owner") = None;
        }
    }
}

struct Wait {
    waiting: metrics::Gauge,
    elapsed: metrics::Histogram,
    start: tokio::time::Instant,
}
impl Drop for Wait {
    fn drop(&mut self) {
        self.waiting.decrement(1.0);
        self.elapsed.record(self.start.elapsed().as_secs_f64());
    }
}

pub(crate) async fn begin_write(db: &Db, caller: &'static str) -> Result<WriteTransaction> {
    begin_with_limit(
        db,
        caller,
        Duration::from_millis(db.timeout_ms.load(Ordering::Relaxed)),
    )
    .await
}

/// Maintenance gets one short opportunity, not the request retry budget.
pub(crate) async fn begin_purge(db: &Db, caller: &'static str) -> Result<WriteTransaction> {
    begin_with_limit(db, caller, Duration::from_millis(100)).await
}

async fn begin_with_limit(
    db: &Db,
    caller: &'static str,
    limit: Duration,
) -> Result<WriteTransaction> {
    db.check_pool_access();
    let start = tokio::time::Instant::now();
    let waiting = metrics::gauge!("tokomotive_db_waiting_writers", "caller" => caller);
    waiting.increment(1.0);
    let _wait = Wait {
        waiting,
        elapsed: metrics::histogram!("tokomotive_db_writer_wait_seconds", "caller" => caller),
        start,
    };
    let acquire = async {
        let permit = db.writer.clone().acquire_owned().await?;
        let mut delay = Duration::from_millis(10);
        loop {
            match db.pool.begin_with("BEGIN IMMEDIATE").await {
                Ok(tx) => {
                    #[cfg(debug_assertions)]
                    {
                        *db.owner.lock().expect("writer owner") = tokio::task::try_id();
                    }
                    return Ok(WriteTransaction {
                        tx: Some(tx),
                        _permit: permit,
                        #[cfg(debug_assertions)]
                        owner: db.owner.clone(),
                    });
                }
                Err(sqlx::Error::Database(e))
                    if e.code()
                        .is_some_and(|c| c.parse::<i32>().is_ok_and(|c| c & 0xff == 5)) =>
                {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(100));
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    tokio::pin!(acquire);
    let warning = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(warning);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut warned = false;
    loop {
        tokio::select! {
            result = &mut acquire => return result,
            _ = &mut warning, if !warned => {
                warned = true;
                tracing::warn!(caller, waited_seconds = start.elapsed().as_secs_f64(), "SQLite writer is still waiting");
                metrics::counter!("tokomotive_db_writer_warnings_total", "caller" => caller).increment(1);
            }
            _ = &mut deadline => {
                metrics::counter!("tokomotive_db_writer_timeouts_total", "caller" => caller).increment(1);
                anyhow::bail!("SQLite writer {caller} exceeded its {} ms acquisition limit", limit.as_millis());
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[tokio::test]
    async fn nested_pool_access_from_writer_task_fails_instead_of_deadlocking() {
        let db = crate::db::connect("sqlite::memory:").await.unwrap();
        let task = tokio::spawn(async move {
            let _tx = super::begin_write(&db, "nested_access_test").await.unwrap();
            sqlx::query("SELECT 1").execute(&db).await.unwrap();
        });
        assert!(task.await.unwrap_err().is_panic());
    }
}
