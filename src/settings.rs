//! Persistent runtime configuration and authenticated secret storage.

use crate::{
    config::{Config, ProviderCfg, RouteCfg, RouterCfg},
    db::Db,
};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub upstream_timeout_secs: u64,
    pub reserve_output_tokens: i64,
    pub session_days: i64,
    pub mode: String,
    pub router: RouterCfg,
    /// Encrypted at rest; omitted from all HTML views.
    pub metrics_token: Option<String>,
}

pub async fn value(db: &Db, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM settings WHERE key=?")
        .bind(key)
        .fetch_optional(db)
        .await?)
}

pub async fn put(db: &Db, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(key).bind(value).execute(db).await?;
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String> {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let mut bytes = [0; 16];
    rand::rng().fill_bytes(&mut bytes);
    let salt = SaltString::encode_b64(&bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

pub async fn seed(db: &Db, cfg: &Config, key: &SecretKey) -> Result<()> {
    if value(db, "runtime_settings").await?.is_none() {
        let settings = Settings {
            upstream_timeout_secs: cfg.server.upstream_timeout_secs,
            reserve_output_tokens: cfg.server.reserve_output_tokens,
            session_days: cfg.server.session_days,
            mode: cfg.server.mode.clone(),
            router: cfg.router.clone(),
            metrics_token: cfg.metrics_token().map(|v| key.encrypt(&v)).transpose()?,
        };
        let mut tx = db.begin().await?;
        for p in &cfg.providers {
            let mut keys = Vec::new();
            if let (Some(env), Some(fallback)) = (&p.api_key_env, &p.api_key) {
                // Resolve the legacy single-key pair once, with env precedence.
                let value = std::env::var(env)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| fallback.clone());
                keys.push(key.encrypt(&value)?);
            } else if let Some(env) = &p.api_key_env {
                keys.push(format!("env:{env}"));
            } else if let Some(v) = &p.api_key {
                keys.push(key.encrypt(v)?);
            }
            for v in &p.api_keys {
                keys.push(key.encrypt(v)?);
            }
            for env in &p.api_keys_env {
                keys.push(format!("env:{env}"));
            }
            sqlx::query(
                "INSERT INTO providers(name,kind,base_url,keys_json,auth,include_usage,key_header) VALUES(?,?,?,?,?,?,?)",
            )
            .bind(&p.name)
            .bind(serde_json::to_string(&p.kind)?)
            .bind(&p.base_url)
            .bind(serde_json::to_string(&keys)?)
            .bind(p.auth.as_str())
            .bind(p.include_usage)
            .bind(p.key_header.map(|h| h.as_str()))
            .execute(&mut *tx)
            .await?;
        }
        for r in &cfg.routes {
            sqlx::query("INSERT INTO routes(name,config_json) VALUES(?,?)")
                .bind(&r.name)
                .bind(serde_json::to_string(r)?)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT INTO settings(key,value) VALUES('runtime_settings',?)")
            .bind(serde_json::to_string(&settings)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    // An explicit environment password remains an operator recovery override.
    let override_password = std::env::var("TOKOMOTIVE_ADMIN_PASSWORD")
        .ok()
        .filter(|v| !v.is_empty());
    if let Some(pw) = override_password {
        put(db, "admin_password_hash", &hash_password(&pw)?).await?;
    } else if value(db, "admin_password_hash").await?.is_none()
        && let Some(pw) = cfg.admin_password()
    {
        put(db, "admin_password_hash", &hash_password(&pw)?).await?;
    }
    Ok(())
}

pub async fn load(db: &Db, key: &SecretKey) -> Result<(Config, Settings)> {
    let mut settings: Settings = serde_json::from_str(
        &value(db, "runtime_settings")
            .await?
            .context("runtime settings missing")?,
    )?;
    settings.metrics_token = settings
        .metrics_token
        .map(|v| key.decrypt(&v))
        .transpose()?;
    let mut cfg: Config = toml::from_str("")?;
    cfg.router = settings.router.clone();
    use sqlx::Row;
    // Runtime validation needs the full configuration; read its cache in batches.
    // Dashboard list handlers use their own bounded queries, not this loader.
    let mut after = String::new();
    loop {
        let rows = sqlx::query("SELECT * FROM providers WHERE name>? ORDER BY name LIMIT 200")
            .bind(&after)
            .fetch_all(db)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = row.get("name");
            let encoded: Vec<String> = serde_json::from_str(row.get("keys_json"))?;
            let mut keys = Vec::new();
            for v in encoded {
                if let Some(env) = v.strip_prefix("env:") {
                    if let Ok(v) = std::env::var(env)
                        && !v.is_empty()
                    {
                        keys.push(v);
                    }
                } else {
                    keys.push(key.decrypt(&v)?);
                }
            }
            cfg.providers.push(ProviderCfg {
                name: row.get("name"),
                kind: serde_json::from_str(row.get("kind"))?,
                auth: serde_json::from_value(serde_json::json!(row.get::<String, _>("auth")))?,
                base_url: row.get("base_url"),
                include_usage: row.get("include_usage"),
                key_header: row
                    .get::<Option<String>, _>("key_header")
                    .map(|h| serde_json::from_value(serde_json::json!(h)))
                    .transpose()?,
                api_key: None,
                api_key_env: None,
                api_keys: keys,
                api_keys_env: vec![],
            });
        }
    }
    let mut after = String::new();
    loop {
        let rows =
            sqlx::query("SELECT name,config_json FROM routes WHERE name>? ORDER BY name LIMIT 200")
                .bind(&after)
                .fetch_all(db)
                .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = row.get("name");
            cfg.routes
                .push(serde_json::from_str::<RouteCfg>(row.get("config_json"))?);
        }
    }
    cfg.validate()?;
    Ok((cfg, settings))
}

#[derive(Clone)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn encrypt(&self, text: &str) -> Result<String> {
        let mut nonce = [0; 12];
        rand::rng().fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("32-byte key");
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), text.as_bytes())
            .map_err(|_| anyhow::anyhow!("cannot encrypt secret"))?;
        Ok(STANDARD.encode([nonce.as_slice(), &encrypted].concat()))
    }

    pub fn decrypt(&self, text: &str) -> Result<String> {
        let bytes = STANDARD.decode(text).context("invalid encrypted secret")?;
        if bytes.len() < 28 {
            bail!("invalid encrypted secret");
        }
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("32-byte key");
        let plain = cipher.decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
            .map_err(|_| anyhow::anyhow!("cannot decrypt stored secret: restore the original TOKOMOTIVE_SECRET_KEY or secret.key backup"))?;
        Ok(String::from_utf8(plain)?)
    }

    pub fn load(database: &str) -> Result<Self> {
        if let Ok(value) = std::env::var("TOKOMOTIVE_SECRET_KEY")
            && !value.trim().is_empty()
        {
            let bytes = if value.len() == 64 {
                hex::decode(&value)?
            } else {
                STANDARD.decode(&value)?
            };
            return Ok(Self(bytes.try_into().map_err(|_| {
                anyhow::anyhow!("TOKOMOTIVE_SECRET_KEY must contain 32 bytes (hex or base64)")
            })?));
        }
        if database.contains(":memory:") {
            let mut bytes = [0; 32];
            rand::rng().fill_bytes(&mut bytes);
            return Ok(Self(bytes));
        }
        let path = database_path(database)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let key_path = parent.join("secret.key");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&key_path) {
            Ok(mut file) => {
                use std::io::Write;
                let mut bytes = [0; 32];
                rand::rng().fill_bytes(&mut bytes);
                file.write_all(STANDARD.encode(bytes).as_bytes())?;
                file.sync_all()?;
                Ok(Self(bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let text = std::fs::read_to_string(&key_path)
                    .with_context(|| format!("read master key {}", key_path.display()))?;
                let bytes = STANDARD.decode(text.trim()).context("invalid secret.key")?;
                Ok(Self(bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("secret.key must contain 32 base64-encoded bytes")
                })?))
            }
            Err(e) => Err(e).context("create secret.key next to the database"),
        }
    }
}

pub fn database_path(database: &str) -> Result<PathBuf> {
    use std::str::FromStr;
    Ok(sqlx::sqlite::SqliteConnectOptions::from_str(database)?
        .get_filename()
        .to_path_buf())
}
