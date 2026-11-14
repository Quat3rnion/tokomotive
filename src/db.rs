//! SQLite storage: seed, catalog load, auth, ledger and dashboard queries.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};

use crate::catalog::{Catalog, ModelEntry, Quote};
use crate::config::{Config, percent_to_bp, usd_to_micros};
use crate::pagination::{Page, PageQuery};
use crate::pricing::{CardRates, PriceSpec};
use crate::ratelimit::Limits;
use crate::usage::Usage;

pub type Db = SqlitePool;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_s() -> i64 {
    now_ms() / 1000
}

pub async fn connect(url: &str) -> Result<Db> {
    if !url.contains(":memory:") {
        let path = crate::settings::database_path(url)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("create database directory")?;
        }
    }
    let opts = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("database url {url}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

// ---------------------------------------------------------------- settings

const DEFAULT_MARKUP: &str = "default_markup_bp";

pub async fn default_markup_bp(db: &Db) -> Result<i64> {
    let v: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(DEFAULT_MARKUP)
        .fetch_optional(db)
        .await?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

pub async fn set_default_markup_bp(db: &Db, bp: i64) -> Result<()> {
    sqlx::query("INSERT INTO settings(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(DEFAULT_MARKUP)
        .bind(bp.to_string())
        .execute(db)
        .await?;
    Ok(())
}

// -------------------------------------------------------------------- seed

/// Add the configured items that are not in the database yet.
/// Existing rows are not changed, so dashboard edits survive a restart.
pub async fn seed(db: &Db, cfg: &Config) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO settings(key, value) VALUES(?, ?)")
        .bind(DEFAULT_MARKUP)
        .bind(percent_to_bp(cfg.server.default_markup_percent).to_string())
        .execute(db)
        .await?;

    for t in &cfg.teams {
        create_team(db, &t.name).await.ok();
    }
    let teams = team_ids(db).await?;

    for m in &cfg.models {
        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM models WHERE name = ?")
            .bind(&m.name)
            .fetch_optional(db)
            .await?;
        if existing.is_some() {
            continue;
        }
        let id = create_model(db, &m.name, &m.provider, &m.aliases).await?;
        add_cost_card(db, id, &m.cost.to_card()).await?;
        if let Some(p) = &m.price {
            add_price_card(db, id, None, &p.to_spec()?).await?;
        }
    }

    for t in &cfg.teams {
        let team_id = teams[&t.name];
        for o in &t.price_overrides {
            let Some(model_id) = model_id(db, &o.model).await? else {
                tracing::warn!(team = %t.name, model = %o.model, "price override for unknown model");
                continue;
            };
            let has: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM cards WHERE model_id = ? AND team_id = ? AND kind = 'price' LIMIT 1",
            )
            .bind(model_id)
            .bind(team_id)
            .fetch_optional(db)
            .await?;
            if has.is_none() {
                add_price_card(db, model_id, Some(team_id), &o.price.to_spec()?).await?;
            }
        }
    }

    for u in &cfg.users {
        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE name = ?")
            .bind(&u.name)
            .fetch_optional(db)
            .await?;
        let user_id = match existing {
            Some(id) => id,
            None => {
                let team = u.team.as_ref().map(|t| teams[t]);
                create_user_with_limits(
                    db,
                    &u.name,
                    team,
                    u.budget_usd.map(usd_to_micros),
                    u.budget_monthly,
                    Limits {
                        rpm: u.rpm_limit,
                        tpm: u.tpm_limit,
                    },
                )
                .await?
            }
        };
        for k in &u.keys {
            sqlx::query(
                "INSERT OR IGNORE INTO virtual_keys(user_id, key_hash, prefix, label, created_at, rpm_limit, tpm_limit, allowed_providers) VALUES(?, ?, ?, 'config', ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(hash_key(k))
            .bind(key_prefix(k))
            .bind(now_s())
            .bind(u.key_rpm_limit)
            .bind(u.key_tpm_limit)
            .bind(serde_json::to_string(&u.key_allowed_providers)?)
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

async fn team_ids(db: &Db) -> Result<HashMap<String, i64>> {
    // Seed-only lookup. Read in bounded batches; routing setup may retain all ids.
    let mut out = HashMap::new();
    let mut after = 0;
    loop {
        let rows = sqlx::query("SELECT id,name FROM teams WHERE id>? ORDER BY id LIMIT 200")
            .bind(after)
            .fetch_all(db)
            .await?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            after = r.get("id");
            out.insert(r.get("name"), after);
        }
    }
    Ok(out)
}

async fn model_id(db: &Db, name: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM models WHERE name = ?")
        .bind(name)
        .fetch_optional(db)
        .await?)
}

// ----------------------------------------------------------------- catalog

pub async fn load_catalog(db: &Db) -> Result<Catalog> {
    // The router needs the full resident catalog, but database reads are bounded.
    // Dashboard list handlers use model_ids_page rather than reloading this cache.
    let mut after = 0;
    let mut entries = Vec::new();
    loop {
        let models = sqlx::query(
            "SELECT id,name,provider,aliases FROM models WHERE id>? ORDER BY id LIMIT 200",
        )
        .bind(after)
        .fetch_all(db)
        .await?;
        if models.is_empty() {
            break;
        }
        after = models.last().expect("nonempty").get("id");
        entries.extend(
            models
                .into_iter()
                .map(|r| ModelEntry {
                    id: r.get("id"),
                    name: r.get("name"),
                    provider: r.get("provider"),
                    aliases: serde_json::from_str(&r.get::<String, _>("aliases"))
                        .unwrap_or_default(),
                    cost: None,
                    price: None,
                    team_price: HashMap::new(),
                })
                .collect::<Vec<_>>(),
        );
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let index: HashMap<i64, usize> = entries.iter().enumerate().map(|(i, m)| (m.id, i)).collect();

    // Current card per (model, team, kind) = highest id.
    let mut after = 0;
    loop {
        let cards = sqlx::query(
            "SELECT c.id,c.model_id,c.team_id,c.kind,c.spec FROM cards c
             WHERE c.id>? AND c.id=(SELECT MAX(c2.id) FROM cards c2
               WHERE c2.model_id=c.model_id AND c2.team_id IS c.team_id AND c2.kind=c.kind)
             ORDER BY c.id LIMIT 200",
        )
        .bind(after)
        .fetch_all(db)
        .await?;
        if cards.is_empty() {
            break;
        }
        for r in cards {
            after = r.get("id");
            let Some(&i) = index.get(&r.get::<i64, _>("model_id")) else {
                continue;
            };
            let id: i64 = r.get("id");
            let spec: String = r.get("spec");
            let team: Option<i64> = r.get("team_id");
            match (r.get::<String, _>("kind").as_str(), team) {
                ("cost", _) => entries[i].cost = Some((id, serde_json::from_str(&spec)?)),
                // A "null" price spec marks a removed Price Card.
                ("price", None) => {
                    entries[i].price =
                        serde_json::from_str::<Option<PriceSpec>>(&spec)?.map(|p| (id, p))
                }
                ("price", Some(t)) => {
                    if let Some(p) = serde_json::from_str::<Option<PriceSpec>>(&spec)? {
                        entries[i].team_price.insert(t, (id, p));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(Catalog {
        models: entries,
        default_markup_bp: default_markup_bp(db).await?,
    })
}

pub async fn create_model(db: &Db, name: &str, provider: &str, aliases: &[String]) -> Result<i64> {
    let id =
        sqlx::query("INSERT INTO models(name, provider, aliases, created_at) VALUES(?, ?, ?, ?)")
            .bind(name)
            .bind(provider)
            .bind(serde_json::to_string(aliases)?)
            .bind(now_s())
            .execute(db)
            .await?
            .last_insert_rowid();
    Ok(id)
}

pub async fn update_model(db: &Db, id: i64, provider: &str, aliases: &[String]) -> Result<()> {
    sqlx::query("UPDATE models SET provider = ?, aliases = ? WHERE id = ?")
        .bind(provider)
        .bind(serde_json::to_string(aliases)?)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn add_cost_card(db: &Db, model_id: i64, rates: &CardRates) -> Result<i64> {
    add_card(db, model_id, None, "cost", serde_json::to_string(rates)?).await
}

pub async fn add_price_card(
    db: &Db,
    model_id: i64,
    team_id: Option<i64>,
    spec: &PriceSpec,
) -> Result<i64> {
    add_card(db, model_id, team_id, "price", serde_json::to_string(spec)?).await
}

async fn add_card(
    db: &Db,
    model_id: i64,
    team_id: Option<i64>,
    kind: &str,
    spec: String,
) -> Result<i64> {
    Ok(sqlx::query(
        "INSERT INTO cards(model_id, team_id, kind, spec, created_at) VALUES(?, ?, ?, ?, ?)",
    )
    .bind(model_id)
    .bind(team_id)
    .bind(kind)
    .bind(spec)
    .bind(now_s())
    .execute(db)
    .await?
    .last_insert_rowid())
}

/// Remove a Price Card (Model or Team override). Card rows are never deleted,
/// so this adds a Card Version with a "null" spec; the next card in the
/// precedence order applies.
pub async fn add_null_price_card(db: &Db, model_id: i64, team_id: Option<i64>) -> Result<i64> {
    add_card(db, model_id, team_id, "price", "null".into()).await
}

// ------------------------------------------------------------------- users

pub async fn create_team(db: &Db, name: &str) -> Result<i64> {
    Ok(
        sqlx::query("INSERT INTO teams(name, created_at) VALUES(?, ?)")
            .bind(name)
            .bind(now_s())
            .execute(db)
            .await?
            .last_insert_rowid(),
    )
}

pub async fn create_user(
    db: &Db,
    name: &str,
    team: Option<i64>,
    budget: Option<i64>,
    monthly: bool,
) -> Result<i64> {
    create_user_with_limits(db, name, team, budget, monthly, Limits::default()).await
}

pub async fn create_user_with_limits(
    db: &Db,
    name: &str,
    team: Option<i64>,
    budget: Option<i64>,
    monthly: bool,
    limits: Limits,
) -> Result<i64> {
    Ok(sqlx::query("INSERT INTO users(name, team_id, budget_micros, budget_monthly, created_at, rpm_limit, tpm_limit) VALUES(?, ?, ?, ?, ?, ?, ?)")
        .bind(name)
        .bind(team)
        .bind(budget)
        .bind(monthly)
        .bind(now_s())
        .bind(limits.rpm)
        .bind(limits.tpm)
        .execute(db)
        .await?
        .last_insert_rowid())
}

pub async fn update_user(
    db: &Db,
    id: i64,
    team: Option<i64>,
    budget: Option<i64>,
    monthly: bool,
    disabled: bool,
) -> Result<()> {
    sqlx::query("UPDATE users SET team_id = ?, budget_micros = ?, budget_monthly = ?, disabled = ? WHERE id = ?")
        .bind(team)
        .bind(budget)
        .bind(monthly)
        .bind(disabled)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn update_user_with_limits(
    db: &Db,
    id: i64,
    team: Option<i64>,
    budget: Option<i64>,
    monthly: bool,
    disabled: bool,
    limits: Limits,
) -> Result<()> {
    sqlx::query("UPDATE users SET team_id = ?, budget_micros = ?, budget_monthly = ?, disabled = ?, rpm_limit = ?, tpm_limit = ? WHERE id = ?")
        .bind(team)
        .bind(budget)
        .bind(monthly)
        .bind(disabled)
        .bind(limits.rpm)
        .bind(limits.tpm)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

// -------------------------------------------------------------------- keys

pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn key_prefix(key: &str) -> String {
    key.chars().take(12).collect()
}

/// Make a new Virtual Key. Returns (id, plain key). The plain key is shown once.
pub async fn create_key(
    db: &Db,
    user_id: i64,
    label: &str,
    allowed: Option<Vec<String>>,
) -> Result<(i64, String)> {
    create_key_with_limits(db, user_id, label, allowed, Limits::default()).await
}

pub async fn create_key_with_limits(
    db: &Db,
    user_id: i64,
    label: &str,
    allowed: Option<Vec<String>>,
    limits: Limits,
) -> Result<(i64, String)> {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    let key = format!("sk-toko-{}", hex::encode(bytes));
    let id = sqlx::query(
        "INSERT INTO virtual_keys(user_id, key_hash, prefix, label, allowed_models, created_at, rpm_limit, tpm_limit) VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(hash_key(&key))
    .bind(key_prefix(&key))
    .bind(label)
    .bind(allowed.map(|a| serde_json::to_string(&a).unwrap_or_default()))
    .bind(now_s())
    .bind(limits.rpm)
    .bind(limits.tpm)
    .execute(db)
    .await?
    .last_insert_rowid();
    Ok((id, key))
}

pub async fn update_key_limits(db: &Db, id: i64, limits: Limits) -> Result<()> {
    sqlx::query("UPDATE virtual_keys SET rpm_limit = ?, tpm_limit = ? WHERE id = ?")
        .bind(limits.rpm)
        .bind(limits.tpm)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn revoke_key(db: &Db, id: i64) -> Result<()> {
    sqlx::query("UPDATE virtual_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(now_s())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct KeyAuth {
    pub key_id: i64,
    pub user_id: i64,
    pub user_name: String,
    pub team_id: Option<i64>,
    pub allowed_models: Vec<String>,
    pub allowed_providers: Vec<String>,
    pub revoked: bool,
    pub user_disabled: bool,
    pub budget_micros: Option<i64>,
    pub budget_monthly: bool,
    pub key_limits: Limits,
    pub user_limits: Limits,
}

pub async fn lookup_key(db: &Db, key: &str) -> Result<Option<KeyAuth>> {
    let row = sqlx::query(
        "SELECT k.id, k.user_id, k.allowed_models, k.allowed_providers, k.revoked_at, u.name, u.team_id, u.disabled, u.budget_micros, u.budget_monthly,
            k.rpm_limit AS key_rpm, k.tpm_limit AS key_tpm, u.rpm_limit AS user_rpm, u.tpm_limit AS user_tpm
         FROM virtual_keys k JOIN users u ON u.id = k.user_id WHERE k.key_hash = ?",
    )
    .bind(hash_key(key))
    .fetch_optional(db)
    .await?;
    let allowed_providers: Vec<String> = match row
        .as_ref()
        .and_then(|r| r.get::<Option<String>, _>("allowed_providers"))
    {
        Some(s) => serde_json::from_str(&s).context("invalid Provider Access")?,
        None => Vec::new(),
    };
    Ok(row.map(|r| KeyAuth {
        key_id: r.get("id"),
        user_id: r.get("user_id"),
        user_name: r.get("name"),
        team_id: r.get("team_id"),
        allowed_providers,
        allowed_models: r
            .get::<Option<String>, _>("allowed_models")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        revoked: r.get::<Option<i64>, _>("revoked_at").is_some(),
        user_disabled: r.get::<i64, _>("disabled") != 0,
        budget_micros: r.get("budget_micros"),
        budget_monthly: r.get::<i64, _>("budget_monthly") != 0,
        key_limits: Limits {
            rpm: r.get("key_rpm"),
            tpm: r.get("key_tpm"),
        },
        user_limits: Limits {
            rpm: r.get("user_rpm"),
            tpm: r.get("user_tpm"),
        },
    }))
}

/// Unix seconds at the start of the current Budget period (UTC).
pub fn budget_period_start(monthly: bool) -> i64 {
    if !monthly {
        return 0;
    }
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc::now();
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap()
        .timestamp()
}

/// Total Price for a User since `since_s`.
pub async fn spend_since(db: &Db, user_id: i64, since_s: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT IFNULL(SUM(price_micros), 0) FROM rollup_hourly WHERE user_id = ? AND hour >= ?",
    )
    .bind(user_id)
    .bind(since_s)
    .fetch_one(db)
    .await?)
}

// ------------------------------------------------------------------ ledger

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub ts_ms: i64,
    pub user_id: i64,
    pub key_id: i64,
    pub team_id: Option<i64>,
    pub provider: String,
    pub subscription: bool,
    pub endpoint: String,
    pub requested_model: Option<String>,
    pub model: Option<String>,
    pub status: u16,
    pub streamed: bool,
    pub usage: Usage,
    pub reported_cost_micros: Option<i64>,
    pub quote: Quote,
    pub estimated: bool,
    pub latency_ms: i64,
    pub ttft_ms: Option<i64>,
    pub client_aborted: bool,
    pub provider_key: Option<i64>,
    pub attempts: i64,
}

/// The model key of a rollup row: the matched Model name, so that dated
/// names such as `gpt-4o-2024-08-06` count as `gpt-4o`.
fn rollup_model(e: &NewEntry) -> String {
    e.quote
        .model_name
        .clone()
        .or_else(|| e.model.clone())
        .or_else(|| e.requested_model.clone())
        .unwrap_or_default()
}

pub async fn insert_entry(db: &Db, e: &NewEntry) -> Result<i64> {
    let mut tx = db.begin().await?;
    let u = &e.usage;
    let q = &e.quote;
    let id = sqlx::query(
        "INSERT INTO ledger(ts_ms, user_id, key_id, team_id, provider, endpoint, requested_model, model, model_id, status, streamed,
            input, output, cache_read, cache_write_5m, cache_write_1h, cost_micros, price_micros, cost_card_id, price_card_id,
            unpriced, estimated, latency_ms, ttft_ms, client_aborted, image_input, image_output, provider_key, attempts, subscription, reported_cost_micros)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(e.ts_ms)
    .bind(e.user_id)
    .bind(e.key_id)
    .bind(e.team_id)
    .bind(&e.provider)
    .bind(&e.endpoint)
    .bind(&e.requested_model)
    .bind(&e.model)
    .bind(q.model_id)
    .bind(e.status as i64)
    .bind(e.streamed)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cache_read)
    .bind(u.cache_write_5m)
    .bind(u.cache_write_1h)
    .bind(q.cost_micros)
    .bind(q.price_micros)
    .bind(q.cost_card_id)
    .bind(q.price_card_id)
    .bind(q.unpriced)
    .bind(e.estimated)
    .bind(e.latency_ms)
    .bind(e.ttft_ms)
    .bind(e.client_aborted)
    .bind(u.image_input)
    .bind(u.image_output)
    .bind(e.provider_key)
    .bind(e.attempts)
    .bind(e.subscription)
    .bind(e.reported_cost_micros)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    let hour = e.ts_ms / 1000 / 3600 * 3600;
    let error = (e.status >= 400) as i64;
    sqlx::query(
        "INSERT INTO rollup_hourly(hour, user_id, model, requests, errors, input, output, cache_read, cache_write_5m, cache_write_1h,
            cost_micros, price_micros, latency_ms_sum, ttft_ms_sum, ttft_count, image_input, image_output)
         VALUES(?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(hour, user_id, model) DO UPDATE SET
            requests = requests + 1, errors = errors + excluded.errors,
            input = input + excluded.input, output = output + excluded.output,
            cache_read = cache_read + excluded.cache_read,
            cache_write_5m = cache_write_5m + excluded.cache_write_5m,
            cache_write_1h = cache_write_1h + excluded.cache_write_1h,
            cost_micros = cost_micros + excluded.cost_micros, price_micros = price_micros + excluded.price_micros,
            latency_ms_sum = latency_ms_sum + excluded.latency_ms_sum,
            ttft_ms_sum = ttft_ms_sum + excluded.ttft_ms_sum, ttft_count = ttft_count + excluded.ttft_count,
            image_input = image_input + excluded.image_input, image_output = image_output + excluded.image_output",
    )
    .bind(hour)
    .bind(e.user_id)
    .bind(rollup_model(e))
    .bind(error)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cache_read)
    .bind(u.cache_write_5m)
    .bind(u.cache_write_1h)
    .bind(q.cost_micros)
    .bind(q.price_micros)
    .bind(e.latency_ms)
    .bind(e.ttft_ms.unwrap_or(0))
    .bind(e.ttft_ms.is_some() as i64)
    .bind(u.image_input)
    .bind(u.image_output)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE virtual_keys SET last_used_at = ? WHERE id = ?")
        .bind(e.ts_ms / 1000)
        .bind(e.key_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

/// Price Unpriced Entries that now match a Model with a Cost Card (ADR 0002
/// exception). Returns the number of entries priced.
pub async fn reprice_unpriced(db: &Db, catalog: &Catalog) -> Result<u64> {
    let mut after = 0_i64;
    let mut n = 0;
    let mut tx = db.begin().await?;
    // Keep the existing all-or-nothing save without retaining all rows or hours.
    // A connection-local table is created and dropped in this transaction.
    sqlx::query("CREATE TEMP TABLE reprice_hours(hour INTEGER PRIMARY KEY)")
        .execute(&mut *tx)
        .await?;
    loop {
        let rows = sqlx::query(
        "SELECT id, ts_ms, user_id, team_id, requested_model, model, input, output, cache_read, cache_write_5m, cache_write_1h,
            image_input, image_output, subscription
         FROM ledger WHERE unpriced = 1 AND id>? ORDER BY id LIMIT 200",
    )
    .bind(after)
    .fetch_all(&mut *tx)
    .await?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            after = r.get("id");
            let usage = Usage {
                input: r.get("input"),
                output: r.get("output"),
                cache_read: r.get("cache_read"),
                cache_write_5m: r.get("cache_write_5m"),
                cache_write_1h: r.get("cache_write_1h"),
                image_input: r.get("image_input"),
                image_output: r.get("image_output"),
            };
            let model: Option<String> = r
                .get::<Option<String>, _>("model")
                .or(r.get("requested_model"));
            let mut q = catalog.quote(model.as_deref(), r.get("team_id"), &usage);
            if r.get::<bool, _>("subscription") {
                q.cost_micros = 0;
                q.cost_card_id = None;
            }
            if q.unpriced {
                continue;
            }
            sqlx::query(
            "UPDATE ledger SET model_id = ?, cost_micros = ?, price_micros = ?, cost_card_id = ?, price_card_id = ?, unpriced = 0 WHERE id = ?",
        )
        .bind(q.model_id)
        .bind(q.cost_micros)
        .bind(q.price_micros)
        .bind(q.cost_card_id)
        .bind(q.price_card_id)
        .bind(r.get::<i64, _>("id"))
        .execute(&mut *tx)
        .await?;
            sqlx::query("INSERT OR IGNORE INTO reprice_hours(hour) VALUES(?)")
                .bind(r.get::<i64, _>("ts_ms") / 1000 / 3600 * 3600)
                .execute(&mut *tx)
                .await?;
            n += 1;
        }
    }
    let mut after_hour = i64::MIN;
    loop {
        let hours: Vec<i64> = sqlx::query_scalar(
            "SELECT hour FROM reprice_hours WHERE hour>? ORDER BY hour LIMIT 200",
        )
        .bind(after_hour)
        .fetch_all(&mut *tx)
        .await?;
        if hours.is_empty() {
            break;
        }
        for hour in hours {
            rebuild_rollup_hour(&mut tx, hour).await?;
            after_hour = hour;
        }
    }
    sqlx::query("DROP TABLE reprice_hours")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(n)
}

async fn rebuild_rollup_hour(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM rollup_hourly WHERE hour = ?")
        .bind(hour)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO rollup_hourly(hour, user_id, model, requests, errors, input, output, cache_read, cache_write_5m,
            cache_write_1h, cost_micros, price_micros, latency_ms_sum, ttft_ms_sum, ttft_count, image_input, image_output)
         SELECT ?1, user_id, COALESCE(m.name, l.model, l.requested_model, ''), COUNT(*), SUM(status >= 400),
            SUM(input), SUM(output), SUM(cache_read), SUM(cache_write_5m), SUM(cache_write_1h),
            SUM(cost_micros), SUM(price_micros), SUM(latency_ms), IFNULL(SUM(ttft_ms), 0), COUNT(ttft_ms),
            SUM(image_input), SUM(image_output)
         FROM ledger l LEFT JOIN models m ON m.id = l.model_id
         WHERE ts_ms >= ?1 * 1000 AND ts_ms < (?1 + 3600) * 1000
         GROUP BY user_id, COALESCE(m.name, l.model, l.requested_model, '')",
    )
    .bind(hour)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// --------------------------------------------------------------- dashboard

#[derive(Debug, Clone, Default, Serialize, FromRow)]
pub struct Totals {
    pub requests: i64,
    pub errors: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub image_input: i64,
    pub image_output: i64,
    pub cost_micros: i64,
    pub price_micros: i64,
    pub latency_ms_sum: i64,
    pub ttft_ms_sum: i64,
    pub ttft_count: i64,
}

const TOTALS_COLS: &str = "IFNULL(SUM(requests),0) AS requests, IFNULL(SUM(errors),0) AS errors,
    IFNULL(SUM(input),0) AS input, IFNULL(SUM(output),0) AS output, IFNULL(SUM(cache_read),0) AS cache_read,
    IFNULL(SUM(cache_write_5m),0) AS cache_write_5m, IFNULL(SUM(cache_write_1h),0) AS cache_write_1h,
    IFNULL(SUM(cost_micros),0) AS cost_micros, IFNULL(SUM(price_micros),0) AS price_micros,
    IFNULL(SUM(latency_ms_sum),0) AS latency_ms_sum, IFNULL(SUM(ttft_ms_sum),0) AS ttft_ms_sum,
    IFNULL(SUM(ttft_count),0) AS ttft_count,
    IFNULL(SUM(image_input),0) AS image_input, IFNULL(SUM(image_output),0) AS image_output";

pub async fn totals_since(db: &Db, since_s: i64) -> Result<Totals> {
    Ok(sqlx::query_as(&format!(
        "SELECT {TOTALS_COLS} FROM rollup_hourly WHERE hour >= ?"
    ))
    .bind(since_s)
    .fetch_one(db)
    .await?)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Bucket {
    pub bucket: i64,
    #[sqlx(flatten)]
    pub t: Totals,
}

/// Totals per bucket (bucket size in seconds, a multiple of 3600).
pub async fn series(db: &Db, since_s: i64, bucket_s: i64) -> Result<Vec<Bucket>> {
    Ok(sqlx::query_as(&format!(
         "SELECT (hour / ?1) * ?1 AS bucket, {TOTALS_COLS} FROM rollup_hourly WHERE hour >= ?2 GROUP BY bucket ORDER BY bucket LIMIT 200"
    ))
    .bind(bucket_s)
    .bind(since_s)
    .fetch_all(db)
    .await?)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Group {
    pub key: String,
    #[sqlx(flatten)]
    pub t: Totals,
    pub attempts: i64,
    pub retried: i64,
}

pub async fn by_model(db: &Db, since_s: i64) -> Result<Vec<Group>> {
    grouped_ledger(
        db,
        since_s,
        "COALESCE(m.name, l.model, l.requested_model, '')",
        "COALESCE(m.name, l.model, l.requested_model, '')",
    )
    .await
}

pub async fn by_user(db: &Db, since_s: i64) -> Result<Vec<Group>> {
    grouped_ledger(db, since_s, "u.name", "u.id").await
}

pub async fn by_team(db: &Db, since_s: i64) -> Result<Vec<Group>> {
    grouped_ledger(db, since_s, "IFNULL(t.name, '(no team)')", "u.team_id").await
}

pub async fn by_provider(db: &Db, since_s: i64) -> Result<Vec<Group>> {
    grouped_ledger(db, since_s, "l.provider", "l.provider").await
}

// Only callers above supply the grouping expression. The Ledger holds Provider
// and attempt data that the hourly rollup does not retain.
async fn grouped_ledger(db: &Db, since_s: i64, key: &str, group: &str) -> Result<Vec<Group>> {
    grouped_page(db, since_s, key, group, "requests", "DESC", 10, 0, None).await
}

pub async fn usage_page(
    db: &Db,
    since_s: i64,
    group: &str,
    sort: &str,
    ascending: bool,
    q: &PageQuery,
    model: Option<&str>,
) -> Result<Vec<Group>> {
    let (key, group) = match group {
        "user" => ("u.name", "u.id"),
        "team" => ("IFNULL(t.name, '(no team)')", "u.team_id"),
        "provider" => ("l.provider", "l.provider"),
        _ => (
            "COALESCE(m.name, l.model, l.requested_model, '')",
            "COALESCE(m.name, l.model, l.requested_model, '')",
        ),
    };
    let sort = match sort {
        "key" | "errors" | "input" | "output" | "cost_micros" | "price_micros" | "attempts"
        | "retried" => sort,
        "latency" => "(latency_ms_sum * 1.0 / requests)",
        "profit" => "(price_micros - cost_micros)",
        _ => "requests",
    };
    grouped_page(
        db,
        since_s,
        key,
        group,
        sort,
        if ascending { "ASC" } else { "DESC" },
        q.limit() + 1,
        q.offset(),
        model,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn grouped_page(
    db: &Db,
    since_s: i64,
    key: &str,
    group: &str,
    sort: &str,
    direction: &str,
    limit: i64,
    offset: i64,
    model: Option<&str>,
) -> Result<Vec<Group>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {key} AS key, COUNT(*) AS requests, SUM(l.status >= 400) AS errors,
            SUM(l.input) AS input, SUM(l.output) AS output, SUM(l.cache_read) AS cache_read,
            SUM(l.cache_write_5m) AS cache_write_5m, SUM(l.cache_write_1h) AS cache_write_1h,
            SUM(l.image_input) AS image_input, SUM(l.image_output) AS image_output,
            SUM(l.cost_micros) AS cost_micros, SUM(l.price_micros) AS price_micros,
            SUM(l.latency_ms) AS latency_ms_sum, IFNULL(SUM(l.ttft_ms),0) AS ttft_ms_sum,
            COUNT(l.ttft_ms) AS ttft_count, SUM(l.attempts) AS attempts, SUM(l.attempts > 1) AS retried
         FROM ledger l INDEXED BY ledger_ts JOIN users u ON u.id = l.user_id LEFT JOIN teams t ON t.id = u.team_id
         LEFT JOIN models m ON m.id = l.model_id
           WHERE l.ts_ms >= ? * 1000 AND (? IS NULL OR l.model = ? OR l.requested_model = ?)
           GROUP BY {group} ORDER BY {sort} {direction}, key ASC, {group} ASC LIMIT ? OFFSET ?"
    ))
    .bind(since_s)
    .bind(model)
    .bind(model)
    .bind(model)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub team_id: Option<i64>,
    pub team: Option<String>,
    pub budget_micros: Option<i64>,
    pub budget_monthly: bool,
    pub disabled: bool,
    pub period_spend: i64,
    pub keys: i64,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
}

pub async fn list_users(db: &Db) -> Result<Vec<UserRow>> {
    let month = budget_period_start(true);
    Ok(sqlx::query_as(
        "SELECT u.id, u.name, u.team_id, t.name AS team, u.budget_micros, u.budget_monthly, u.disabled, u.rpm_limit, u.tpm_limit,
            (SELECT IFNULL(SUM(price_micros), 0) FROM rollup_hourly r WHERE r.user_id = u.id
               AND r.hour >= CASE WHEN u.budget_monthly THEN ? ELSE 0 END) AS period_spend,
            (SELECT COUNT(*) FROM virtual_keys k WHERE k.user_id = u.id AND k.revoked_at IS NULL) AS keys
         FROM users u LEFT JOIN teams t ON t.id = u.team_id ORDER BY u.name LIMIT 200",
    )
    .bind(month)
    .fetch_all(db)
    .await?)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TeamRow {
    pub id: i64,
    pub name: String,
    pub users: i64,
}

pub async fn list_teams(db: &Db) -> Result<Vec<TeamRow>> {
    Ok(sqlx::query_as(
        "SELECT t.id, t.name, (SELECT COUNT(*) FROM users u WHERE u.team_id = t.id) AS users FROM teams t ORDER BY t.name LIMIT 200",
    )
    .fetch_all(db)
    .await?)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct KeyRow {
    pub id: i64,
    pub user: String,
    pub prefix: String,
    pub label: String,
    pub allowed_models: Option<String>,
    pub allowed_providers: Option<String>,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
}

pub async fn list_keys(db: &Db) -> Result<Vec<KeyRow>> {
    Ok(sqlx::query_as(
        "SELECT k.id, u.name AS user, k.prefix, k.label, k.allowed_models, k.allowed_providers, k.revoked_at, k.last_used_at, k.created_at, k.rpm_limit, k.tpm_limit
         FROM virtual_keys k JOIN users u ON u.id = k.user_id ORDER BY k.revoked_at IS NOT NULL, u.name, k.id LIMIT 200",
    )
    .fetch_all(db)
    .await?)
}

pub async fn update_key_providers(db: &Db, id: i64, providers: &[String]) -> Result<()> {
    sqlx::query("UPDATE virtual_keys SET allowed_providers=? WHERE id=?")
        .bind(serde_json::to_string(providers)?)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct LedgerRow {
    pub id: i64,
    pub ts_ms: i64,
    pub user: String,
    pub provider: String,
    pub endpoint: String,
    pub requested_model: Option<String>,
    pub model: Option<String>,
    pub status: i64,
    pub streamed: bool,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub cost_micros: i64,
    pub reported_cost_micros: Option<i64>,
    pub price_micros: i64,
    pub unpriced: bool,
    pub estimated: bool,
    pub latency_ms: i64,
    pub ttft_ms: Option<i64>,
    pub client_aborted: bool,
    pub image_input: i64,
    pub image_output: i64,
    pub provider_key: Option<i64>,
    pub attempts: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LedgerFilter {
    #[default]
    All,
    Unpriced,
    Estimated,
    Errors,
}

pub async fn recent_ledger(db: &Db, filter: LedgerFilter, limit: i64) -> Result<Vec<LedgerRow>> {
    ledger_page(
        db,
        &LedgerQuery {
            filter,
            limit,
            ..Default::default()
        },
    )
    .await
}

/// One page of the Ledger, newest first. All conditions are optional.
#[derive(Debug, Clone)]
pub struct LedgerQuery {
    pub filter: LedgerFilter,
    pub user_id: Option<i64>,
    pub provider: Option<String>,
    /// Matches the upstream Model or the requested Model name.
    pub model: Option<String>,
    /// Inclusive lower bound, Unix milliseconds.
    pub from_ms: Option<i64>,
    /// Exclusive upper bound, Unix milliseconds.
    pub to_ms: Option<i64>,
    /// Only entries with a smaller id (the next, older page).
    pub before_id: Option<i64>,
    pub after_id: Option<i64>,
    pub limit: i64,
}

impl Default for LedgerQuery {
    fn default() -> Self {
        Self {
            filter: LedgerFilter::All,
            user_id: None,
            provider: None,
            model: None,
            from_ms: None,
            to_ms: None,
            before_id: None,
            after_id: None,
            limit: 100,
        }
    }
}

pub async fn ledger_page(db: &Db, q: &LedgerQuery) -> Result<Vec<LedgerRow>> {
    let mut bounded = q.clone();
    bounded.limit = q.limit.clamp(1, 200);
    ledger_window(db, &bounded).await
}

/// One extra row is private lookahead, never rendered or returned in an API list.
pub(crate) async fn ledger_window(db: &Db, q: &LedgerQuery) -> Result<Vec<LedgerRow>> {
    let mut b = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT l.id, l.ts_ms, u.name AS user, l.provider, l.endpoint, l.requested_model, l.model, l.status, l.streamed,
            l.input, l.output, l.cache_read, l.cache_write_5m, l.cache_write_1h, l.cost_micros, l.price_micros,
            l.unpriced, l.estimated, l.latency_ms, l.ttft_ms, l.client_aborted,
            l.image_input, l.image_output, l.provider_key, l.attempts, l.reported_cost_micros
         FROM ledger l JOIN users u ON u.id = l.user_id WHERE ",
    );
    b.push(match q.filter {
        LedgerFilter::All => "l.id > 0",
        LedgerFilter::Unpriced => "l.unpriced = 1",
        LedgerFilter::Estimated => "l.estimated = 1",
        LedgerFilter::Errors => "l.status >= 400",
    });
    if let Some(v) = q.user_id {
        b.push(" AND l.user_id = ").push_bind(v);
    }
    if let Some(v) = &q.provider {
        b.push(" AND l.provider = ").push_bind(v.clone());
    }
    if let Some(v) = &q.model {
        b.push(" AND (l.model = ")
            .push_bind(v.clone())
            .push(" OR l.requested_model = ")
            .push_bind(v.clone())
            .push(")");
    }
    if let Some(v) = q.from_ms {
        b.push(" AND l.ts_ms >= ").push_bind(v);
    }
    if let Some(v) = q.to_ms {
        b.push(" AND l.ts_ms < ").push_bind(v);
    }
    if let Some(v) = q.before_id {
        b.push(" AND l.id < ").push_bind(v);
    }
    if let Some(v) = q.after_id {
        b.push(" AND l.id > ").push_bind(v);
    }
    b.push(if q.after_id.is_some() {
        " ORDER BY l.id ASC LIMIT "
    } else {
        " ORDER BY l.id DESC LIMIT "
    })
    .push_bind(q.limit.clamp(1, 201));
    Ok(b.build_query_as().fetch_all(db).await?)
}

/// The Model names of Unpriced Entries, most frequent first.
pub async fn unpriced_models(db: &Db) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT IFNULL(model, IFNULL(requested_model, '')) AS m FROM ledger WHERE unpriced = 1
         GROUP BY m ORDER BY COUNT(*) DESC, m LIMIT 20",
    )
    .fetch_all(db)
    .await?)
}

pub async fn count_flags(db: &Db) -> Result<(i64, i64)> {
    let r = sqlx::query(
        "SELECT IFNULL(SUM(unpriced), 0) AS u, IFNULL(SUM(estimated), 0) AS e FROM ledger",
    )
    .fetch_one(db)
    .await?;
    Ok((r.get("u"), r.get("e")))
}

/// Unpriced and Estimated Entries since `since_s` (Unix seconds).
pub async fn count_flags_since(db: &Db, since_s: i64) -> Result<(i64, i64)> {
    let r = sqlx::query(
        "SELECT IFNULL(SUM(unpriced), 0) AS u, IFNULL(SUM(estimated), 0) AS e FROM ledger WHERE ts_ms >= ?",
    )
    .bind(since_s * 1000)
    .fetch_one(db)
    .await?;
    Ok((r.get("u"), r.get("e")))
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CardHistoryRow {
    pub id: i64,
    pub team: Option<String>,
    pub kind: String,
    pub spec: String,
    pub created_at: i64,
}

pub async fn card_history(db: &Db, model_id: i64) -> Result<Vec<CardHistoryRow>> {
    Ok(sqlx::query_as(
        "SELECT c.id, t.name AS team, c.kind, c.spec, c.created_at FROM cards c LEFT JOIN teams t ON t.id = c.team_id
         WHERE c.model_id = ? ORDER BY c.id DESC LIMIT 20",
    )
    .bind(model_id)
    .fetch_all(db)
    .await?)
}

/// Append-heavy tables use immutable ids, not names that an Operator can edit.
pub async fn users_page(db: &Db, q: &PageQuery, team: Option<i64>) -> Result<Page<UserRow>> {
    let mut b = sqlx::QueryBuilder::new(
        "SELECT u.id, u.name, u.team_id, t.name AS team, u.budget_micros, u.budget_monthly, u.disabled, u.rpm_limit, u.tpm_limit,
        (SELECT IFNULL(SUM(price_micros),0) FROM rollup_hourly r WHERE r.user_id=u.id AND r.hour >= CASE WHEN u.budget_monthly THEN ");
    b.push_bind(budget_period_start(true)).push(
        " ELSE 0 END) AS period_spend,
        (SELECT COUNT(*) FROM virtual_keys k WHERE k.user_id=u.id AND k.revoked_at IS NULL) AS keys
        FROM users u LEFT JOIN teams t ON t.id=u.team_id WHERE instr(lower(u.name),lower(",
    );
    b.push_bind(q.q.clone()).push(")) > 0");
    if let Some(t) = team {
        b.push(" AND u.team_id=").push_bind(t);
    }
    q.keyset(&mut b, "u.id");
    Ok(Page::new(
        b.build_query_as().fetch_all(db).await?,
        q,
        |r: &UserRow| r.id,
    ))
}

pub async fn teams_page(db: &Db, q: &PageQuery) -> Result<Page<TeamRow>> {
    let mut b = sqlx::QueryBuilder::new(
        "SELECT t.id,t.name,(SELECT COUNT(*) FROM users u WHERE u.team_id=t.id) AS users FROM teams t WHERE instr(lower(t.name),lower(",
    );
    b.push_bind(q.q.clone()).push(")) > 0");
    q.keyset(&mut b, "t.id");
    Ok(Page::new(
        b.build_query_as().fetch_all(db).await?,
        q,
        |r: &TeamRow| r.id,
    ))
}

pub async fn keys_page(
    db: &Db,
    q: &PageQuery,
    user: Option<i64>,
    revoked: bool,
) -> Result<Page<KeyRow>> {
    let mut b = sqlx::QueryBuilder::new(
        "SELECT k.id,u.name AS user,k.prefix,k.label,k.allowed_models,k.allowed_providers,k.revoked_at,k.last_used_at,k.created_at,k.rpm_limit,k.tpm_limit FROM virtual_keys k JOIN users u ON u.id=k.user_id WHERE ",
    );
    b.push(if revoked {
        "k.revoked_at IS NOT NULL"
    } else {
        "k.revoked_at IS NULL"
    });
    if let Some(u) = user {
        b.push(" AND k.user_id=").push_bind(u);
    }
    b.push(" AND (instr(lower(k.label),lower(")
        .push_bind(q.q.clone())
        .push(")) > 0 OR instr(lower(u.name),lower(")
        .push_bind(q.q.clone())
        .push(")) > 0)");
    q.keyset(&mut b, "k.id");
    Ok(Page::new(
        b.build_query_as().fetch_all(db).await?,
        q,
        |r: &KeyRow| r.id,
    ))
}

pub async fn override_teams_page(db: &Db, model: i64, q: &PageQuery) -> Result<Page<TeamRow>> {
    let mut b = sqlx::QueryBuilder::new(
        "SELECT t.id,t.name,0 AS users FROM teams t WHERE EXISTS(SELECT 1 FROM cards c WHERE c.team_id=t.id AND c.model_id=",
    );
    b.push_bind(model).push(" AND c.kind='price' AND c.spec!='null' AND c.id=(SELECT MAX(c2.id) FROM cards c2 WHERE c2.team_id=t.id AND c2.model_id=c.model_id AND c2.kind='price'))");
    q.keyset(&mut b, "t.id");
    Ok(Page::new(
        b.build_query_as().fetch_all(db).await?,
        q,
        |r: &TeamRow| r.id,
    ))
}

pub async fn history_page(db: &Db, model: i64, q: &PageQuery) -> Result<Page<CardHistoryRow>> {
    let mut b = sqlx::QueryBuilder::new(
        "SELECT c.id,t.name AS team,c.kind,c.spec,c.created_at FROM cards c LEFT JOIN teams t ON t.id=c.team_id WHERE c.model_id=",
    );
    b.push_bind(model);
    q.keyset_desc(&mut b, "c.id");
    Ok(Page::new(
        b.build_query_as().fetch_all(db).await?,
        q,
        |r: &CardHistoryRow| r.id,
    ))
}

pub async fn model_ids_page(db: &Db, q: &PageQuery, team: Option<i64>) -> Result<Page<i64>> {
    let mut b =
        sqlx::QueryBuilder::new("SELECT m.id FROM models m WHERE instr(lower(m.name),lower(");
    b.push_bind(q.q.clone()).push(")) > 0");
    if let Some(t) = team {
        b.push(" AND EXISTS(SELECT 1 FROM cards c WHERE c.model_id=m.id AND c.team_id=").push_bind(t)
            .push(" AND c.kind='price' AND c.spec!='null' AND c.id=(SELECT MAX(c2.id) FROM cards c2 WHERE c2.model_id=c.model_id AND c2.team_id=c.team_id AND c2.kind='price'))");
    }
    q.keyset(&mut b, "m.id");
    Ok(Page::new(
        b.build_query_scalar().fetch_all(db).await?,
        q,
        |id| *id,
    ))
}

#[derive(Debug, Serialize, FromRow)]
pub struct Suggestion {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ControlChoice {
    pub name: String,
    pub subscription: bool,
}

impl ControlChoice {
    pub fn selected(&self, names: &str) -> bool {
        names.split(',').any(|name| name.trim() == self.name)
    }
}

/// A 31-row probe distinguishes small admin sets without loading a large set.
pub async fn small_choices(db: &Db, kind: &str) -> Result<Option<Vec<ControlChoice>>> {
    let sql = match kind {
        "providers" => {
            "SELECT name,auth IN ('chatgpt','claude','codex') AS subscription FROM providers WHERE auth != 'google_vertex' ORDER BY name LIMIT 31"
        }
        "teams" => "SELECT name,0 AS subscription FROM teams ORDER BY name LIMIT 31",
        _ => anyhow::bail!("unsupported control choices"),
    };
    let choices: Vec<ControlChoice> = sqlx::query_as(sql).fetch_all(db).await?;
    Ok((choices.len() <= 30).then_some(choices))
}

/// These are the only accepted tables. Bind the search term, never SQL text.
pub async fn search_names(db: &Db, kind: &str, q: &str) -> Result<Vec<Suggestion>> {
    let table = match kind {
        "users" => "users",
        "teams" => "teams",
        "providers" => "providers",
        "routes" => "routes",
        _ => "models",
    };
    let id = if matches!(table, "providers" | "routes") {
        "rowid"
    } else {
        "id"
    };
    let routable = if table == "providers" {
        " AND auth != 'google_vertex'"
    } else {
        ""
    };
    Ok(sqlx::query_as(&format!("SELECT {id} AS id,name FROM {table} WHERE instr(lower(name),lower(?)) > 0{routable} ORDER BY name LIMIT 20"))
        .bind(q).fetch_all(db).await?)
}

/// Prefer an exact name; legacy numeric form values remain valid only if the id exists.
pub async fn resolve_name(db: &Db, kind: &str, value: &str, allow_id: bool) -> Result<Option<i64>> {
    let table = if kind == "teams" { "teams" } else { "users" };
    Ok(sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE name=? OR id=? ORDER BY name=? DESC LIMIT 1"
    ))
    .bind(value.trim())
    .bind(if allow_id {
        value.trim().parse::<i64>().ok()
    } else {
        None
    })
    .bind(value.trim())
    .fetch_optional(db)
    .await?)
}

// ---------------------------------------------------------------- sessions

/// Store a new dashboard session. Only the hash of the token is stored.
pub async fn create_session(db: &Db, token: &str, days: i64) -> Result<()> {
    let password = crate::settings::value(db, "admin_password_hash").await?;
    create_session_for_password(db, token, days, password.as_deref()).await
}

/// Use the exact hash that the login verified, not a fresh password lookup.
pub async fn create_session_for_password(
    db: &Db,
    token: &str,
    days: i64,
    password_hash: Option<&str>,
) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(now_s())
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO sessions(token_hash, expires_at, password_hash) VALUES(?, ?, ?)")
        .bind(hash_key(token))
        .bind(now_s() + days * 86_400)
        .bind(password_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn session_valid(db: &Db, token: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE token_hash = ? AND expires_at >= ?
         AND password_hash IS (SELECT value FROM settings WHERE key='admin_password_hash')",
    )
    .bind(hash_key(token))
    .bind(now_s())
    .fetch_one(db)
    .await?;
    Ok(n > 0)
}

pub async fn delete_session(db: &Db, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(hash_key(token))
        .execute(db)
        .await?;
    Ok(())
}
