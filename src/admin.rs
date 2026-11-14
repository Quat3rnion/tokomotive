//! Dashboard identity. Sessions carry the version and hash verified at login.
use crate::db::{self, Db};
use anyhow::Result;

#[derive(Clone, sqlx::FromRow)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub password_hash: String,
    pub session_version: i64,
}

pub async fn configured(db: &Db) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_accounts)")
            .fetch_one(db)
            .await?,
    )
}
pub async fn by_username(db: &Db, username: &str) -> Result<Option<Account>> {
    Ok(sqlx::query_as("SELECT id,username,role,password_hash,session_version FROM admin_accounts WHERE username=?")
        .bind(username).fetch_optional(db).await?)
}
pub async fn session(db: &Db, token: &str) -> Result<Option<Account>> {
    Ok(sqlx::query_as("SELECT a.id,a.username,a.role,a.password_hash,a.session_version FROM sessions s JOIN admin_accounts a ON a.id=s.account_id WHERE s.token_hash=? AND s.expires_at>=? AND s.password_hash=a.password_hash AND s.account_version=a.session_version")
        .bind(db::hash_key(token)).bind(db::now_s()).fetch_optional(db).await?)
}
pub async fn create_session(db: &Db, token: &str, days: i64, account: &Account) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at<?")
        .bind(db::now_s())
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO sessions(token_hash,expires_at,password_hash,account_id,account_version) VALUES(?,?,?,?,?)")
        .bind(db::hash_key(token)).bind(db::now_s()+days*86_400).bind(&account.password_hash)
        .bind(account.id).bind(account.session_version).execute(db).await?;
    Ok(())
}

pub fn username(value: &str) -> Result<&str> {
    let value = value.trim();
    let value = if value.is_empty() { "admin" } else { value };
    anyhow::ensure!(
        value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "Use 1 to 64 letters, digits, dots, underscores or hyphens for the username."
    );
    Ok(value)
}

/// Remove entries older than the configured retention period (365 days by default).
pub async fn purge_audit(db: &Db, now: i64) -> Result<u64> {
    let days = crate::settings::value(db, "audit_retention_days")
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(365)
        .clamp(1, 36500);
    let mut total = 0;
    loop {
        let removed = sqlx::query("DELETE FROM admin_audit WHERE id IN (SELECT id FROM admin_audit WHERE time<? ORDER BY time,id LIMIT 5000)")
            .bind(now - days * 86400).execute(db).await?.rows_affected();
        total += removed;
        if removed < 5000 {
            return Ok(total);
        }
        tokio::task::yield_now().await;
    }
}
pub fn start_audit_purge(db: Db) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = purge_audit(&db, db::now_s()).await {
                tracing::error!(error=%e,"audit retention purge failed");
            }
        }
    })
}
