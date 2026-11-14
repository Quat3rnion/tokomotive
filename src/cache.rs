//! Exact responses. Only complete client-format bodies are stored, encrypted.
use crate::{
    db::{self, Db},
    settings::SecretKey,
    usage::Usage,
};
use anyhow::Result;
use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub scope: Scope,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: 300,
            scope: Scope::Key,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Key,
    Team,
    Global,
}
impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Team => "team",
            Self::Global => "global",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_entries: u64,
    pub max_bytes: u64,
    pub max_entry_bytes: u64,
    pub hit_price_micros: i64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_bytes: 100 * 1024 * 1024,
            max_entry_bytes: 1024 * 1024,
            hit_price_micros: 0,
        }
    }
}
#[derive(Clone)]
pub struct Plan {
    pub key: String,
    pub ttl_secs: u64,
    pub lookup: bool,
}
impl Plan {
    /// Drop only top-level transport and attribution fields. Nested user/metadata
    /// values can affect tool arguments, and must remain in the digest.
    pub fn new(
        policy: &Policy,
        route: &str,
        endpoint: &str,
        body: &Value,
        headers: &HeaderMap,
        key_id: i64,
        team_id: Option<i64>,
    ) -> Option<Self> {
        if !policy.enabled
            || policy.ttl_secs == 0
            || body
                .get("n")
                .and_then(Value::as_f64)
                .is_some_and(|n| n > 1.0)
        {
            return None;
        }
        if headers
            .get("x-tokomotive-cache")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("bypass"))
        {
            return None;
        }
        let directives: Vec<_> = headers
            .get_all("cache-control")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|v| {
                v.trim()
                    .split('=')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            })
            .collect();
        if directives.iter().any(|s| s == "no-store") {
            return None;
        }
        let scope = match policy.scope {
            Scope::Key => format!("key:{key_id}"),
            Scope::Team => format!("team:{}", team_id?),
            Scope::Global => "global".into(),
        };
        let mut normalized = body.clone();
        if let Some(obj) = normalized.as_object_mut() {
            for field in ["stream", "stream_options", "user", "metadata"] {
                obj.remove(field);
            }
        }
        // serde_json uses sorted maps (preserve_order is not enabled).
        // SSE is a separate representation, including requested usage chunks.
        let identity = serde_json::json!([
            endpoint,
            route,
            scope,
            normalized,
            body["stream"] == true,
            body["stream_options"]["include_usage"] == true,
            (["anthropic-version", "anthropic-beta", "openai-beta"]
                .map(|name| headers.get(name).and_then(|v| v.to_str().ok())))
        ]);
        Some(Self {
            key: hex::encode(Sha256::digest(identity.to_string())),
            ttl_secs: policy.ttl_secs,
            lookup: !directives.iter().any(|s| s == "no-cache"),
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub body: String,
    pub content_type: String,
    pub provider: String,
    pub upstream_model: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    pub estimated: bool,
}
impl Entry {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(STANDARD.decode(&self.body)?)
    }
    pub fn set_body(&mut self, bytes: &[u8]) {
        self.body = STANDARD.encode(bytes);
    }
}
pub struct Write {
    pub st: crate::state::AppState,
    pub plan: Plan,
    pub entry: Entry,
}
impl Write {
    pub async fn store(mut self, bytes: &[u8], content_type: &str) {
        let limits = self.st.settings().cache.clone();
        if bytes.len() as u64 > limits.max_entry_bytes {
            return;
        }
        self.entry.set_body(bytes);
        self.entry.content_type = content_type.into();
        if let Err(e) = store(
            &self.st.db,
            &self.st.secret_key,
            &self.plan,
            &limits,
            &self.entry,
        )
        .await
        {
            tracing::warn!(error=%e,"cache store failed");
        }
    }
}
pub async fn lookup(
    db: &Db,
    secret: &SecretKey,
    plan: &Plan,
    limits: &Limits,
) -> Result<Option<Entry>> {
    if !plan.lookup || limits.max_entries == 0 || limits.max_bytes == 0 {
        return Ok(None);
    }
    let raw: Option<String> = sqlx::query_scalar("SELECT body FROM response_cache WHERE key=? AND expires_ms>? AND body_bytes<=? AND body_bytes<=?")
        .bind(&plan.key).bind(db::now_ms()).bind(limits.max_entry_bytes.min(i64::MAX as u64) as i64).bind(limits.max_bytes.min(i64::MAX as u64) as i64).fetch_optional(db).await?;
    raw.map(|s| {
        let (identity, entry): (String, Entry) = serde_json::from_str(&secret.decrypt(&s)?)?;
        anyhow::ensure!(identity == plan.key, "cache identity mismatch");
        entry.bytes()?;
        Ok(entry)
    })
    .transpose()
}
pub async fn store(
    db: &Db,
    secret: &SecretKey,
    plan: &Plan,
    limits: &Limits,
    entry: &Entry,
) -> Result<()> {
    if limits.max_entries == 0
        || entry.body.len() as u64 > limits.max_entry_bytes
        || limits.max_bytes == 0
    {
        return Ok(());
    }
    // Authenticate the scope/request identity inside the encrypted envelope.
    // Moving a ciphertext to a different cache row must not create a hit.
    let body = secret.encrypt(&serde_json::to_string(&(&plan.key, entry))?)?;
    // Include encryption, metadata and base64 overhead in both byte limits.
    let size = body.len() as u64;
    if size > limits.max_entry_bytes || size > limits.max_bytes {
        return Ok(());
    }
    let now = db::now_ms();
    let expires = now.saturating_add(plan.ttl_secs.min(i64::MAX as u64 / 1000) as i64 * 1000);
    let mut tx = db.begin().await?;
    sqlx::query("INSERT OR REPLACE INTO response_cache(key,body,body_bytes,created_ms,expires_ms) VALUES(?,?,?,?,?)")
        .bind(&plan.key).bind(body).bind(size as i64).bind(now).bind(expires).execute(&mut *tx).await?;
    prune(&mut tx, limits).await?;
    tx.commit().await?;
    metrics::counter!("llm_cache_stored_bytes_total").increment(size);
    Ok(())
}
pub async fn purge(db: &Db) -> Result<()> {
    sqlx::query("DELETE FROM response_cache WHERE expires_ms<=?")
        .bind(db::now_ms())
        .execute(db)
        .await?;
    Ok(())
}
async fn prune(conn: &mut sqlx::SqliteConnection, limits: &Limits) -> Result<()> {
    sqlx::query("DELETE FROM response_cache WHERE expires_ms<=? OR body_bytes>?")
        .bind(db::now_ms())
        .bind(limits.max_entry_bytes.min(i64::MAX as u64) as i64)
        .execute(&mut *conn)
        .await?;
    // Newest entries survive. rowid breaks ties within the same millisecond.
    sqlx::query("DELETE FROM response_cache WHERE rowid IN (SELECT rowid FROM (SELECT rowid,ROW_NUMBER() OVER (ORDER BY created_ms DESC,rowid DESC) AS n,SUM(body_bytes) OVER (ORDER BY created_ms DESC,rowid DESC) AS bytes FROM response_cache) WHERE n>? OR bytes>?)")
        .bind(limits.max_entries.min(i64::MAX as u64) as i64).bind(limits.max_bytes.min(i64::MAX as u64) as i64).execute(conn).await?;
    Ok(())
}
pub async fn maintain(db: &Db, limits: &Limits) -> Result<()> {
    let mut tx = db.begin().await?;
    prune(&mut tx, limits).await?;
    tx.commit().await?;
    Ok(())
}
pub fn start_purge(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let db = st.db.clone();
            let limits = st.settings().cache.clone();
            st.spawn_tracked(async move {
                if let Err(e) = maintain(&db, &limits).await {
                    tracing::warn!(error=%e,"cache purge failed");
                }
            });
        }
    })
}
/// Accumulate translated SSE with a hard memory cap. Only a terminal event
/// without an error can populate the cache; EOF and final usage are not enough.
pub struct StreamBody {
    bytes: Vec<u8>,
    cap: usize,
    overflow: bool,
}
impl StreamBody {
    pub fn new(cap: u64) -> Self {
        Self {
            bytes: Vec::new(),
            cap: cap.min(usize::MAX as u64) as usize,
            overflow: false,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.overflow {
            return;
        }
        if self.bytes.len().saturating_add(bytes.len()) > self.cap {
            self.bytes.clear();
            self.overflow = true;
        } else {
            self.bytes.extend_from_slice(bytes);
        }
    }
    pub fn complete(self, endpoint: &str) -> Option<Vec<u8>> {
        if self.overflow {
            return None;
        }
        let text = std::str::from_utf8(&self.bytes).ok()?;
        let mut terminal = false;
        let mut finished = false;
        let mut started = false;
        if text.lines().any(|line| {
            line.strip_prefix("event:")
                .is_some_and(|kind| kind.trim() == "error")
        }) {
            return None;
        }
        for line in text.lines().filter_map(|s| s.strip_prefix("data:")) {
            let data = line.trim();
            if data == "[DONE]" {
                terminal |= endpoint == "chat_completions";
                continue;
            }
            let v: Value = serde_json::from_str(data).ok()?;
            if !v.is_object() {
                return None;
            }
            let kind = v["type"].as_str().unwrap_or_default();
            if v.get("error").is_some()
                || matches!(kind, "error" | "response.failed" | "response.incomplete")
                || matches!(
                    v["response"]["status"].as_str(),
                    Some("failed" | "incomplete" | "cancelled")
                )
            {
                return None;
            }
            terminal |= (endpoint == "messages" && kind == "message_stop")
                || (endpoint == "responses" && kind == "response.completed");
            started |= kind == "message_start";
            finished |= (endpoint == "messages" && v["delta"]["stop_reason"].is_string())
                || (endpoint == "chat_completions"
                    && v["choices"].as_array().is_some_and(|choices| {
                        !choices.is_empty()
                            && choices
                                .iter()
                                .all(|choice| choice["finish_reason"].is_string())
                    }))
                || (endpoint == "responses"
                    && kind == "response.completed"
                    && v["response"]["status"] == "completed");
        }
        (terminal && finished && (endpoint != "messages" || started)).then_some(self.bytes)
    }
}

pub fn complete_json(endpoint: &str, bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).is_ok_and(|v| {
        if !v.is_object() || v.get("error").is_some() {
            return false;
        }
        match endpoint {
            "chat_completions" => v["choices"].as_array().is_some_and(|choices| {
                !choices.is_empty()
                    && choices.iter().all(|choice| {
                        choice["finish_reason"].is_string() && choice["message"].is_object()
                    })
            }),
            "messages" => {
                v["type"] == "message" && v["content"].is_array() && v["stop_reason"].is_string()
            }
            "responses" => v["status"] == "completed" && v["output"].is_array(),
            "embeddings" => v["data"].as_array().is_some_and(|data| {
                !data.is_empty()
                    && data
                        .iter()
                        .all(|item| item["embedding"].is_array() || item["embedding"].is_string())
            }),
            _ => false,
        }
    })
}
