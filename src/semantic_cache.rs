//! Opt-in semantic matching. Vectors and responses use the same encrypted,
//! bounded store as exact responses. Unknown request fields stay in the bucket.
use crate::{cache, db::Db, settings::SecretKey};
use anyhow::Result;
use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub enabled: bool,
    pub embedding_model: String,
    pub threshold: f64,
    pub input: Input,
    pub max_chars: usize,
    pub scope: cache::Scope,
    pub ttl_secs: u64,
    pub max_vectors: usize,
    pub timeout_ms: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding_model: String::new(),
            threshold: 0.95,
            input: Input::LastUser,
            max_chars: 16_000,
            scope: cache::Scope::Key,
            ttl_secs: 300,
            max_vectors: 5000,
            timeout_ms: 2000,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.threshold.is_finite() && (0.0..=1.0).contains(&self.threshold),
            "Semantic similarity must be between 0 and 1."
        );
        anyhow::ensure!(
            (1..=5000).contains(&self.max_vectors),
            "Semantic search limit must be between 1 and 5000."
        );
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.max_chars),
            "Semantic input limit must be between 1 and 1000000 characters."
        );
        anyhow::ensure!(
            (1..=60_000).contains(&self.timeout_ms),
            "Semantic timeout must be between 1 and 60000 milliseconds."
        );
        anyhow::ensure!(
            self.ttl_secs > 0,
            "Semantic lifetime must be at least 1 second."
        );
        anyhow::ensure!(
            !self.enabled || !self.embedding_model.trim().is_empty(),
            "Choose a semantic embedding model."
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    #[default]
    LastUser,
    Conversation,
}
impl Input {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LastUser => "last_user",
            Self::Conversation => "conversation",
        }
    }
}
#[derive(Clone)]
pub struct Plan {
    pub exact: cache::Plan,
    pub bucket: String,
    pub text: String,
    pub policy: Policy,
}
#[derive(Clone)]
pub struct WritePlan {
    pub plan: Plan,
    pub vector: Vec<f32>,
}
pub(crate) struct Embedding {
    pub vector: Vec<f32>,
    pub identity: String,
}
impl Plan {
    pub(crate) fn bind_embedding(&mut self, identity: &str) {
        self.bucket = hex::encode(Sha256::digest(json!([self.bucket, identity]).to_string()));
        self.exact.key = format!(
            "semantic:{}",
            hex::encode(Sha256::digest(
                json!([self.exact.key, identity]).to_string()
            ))
        );
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        policy: &Policy,
        route: &str,
        endpoint: &str,
        body: &Value,
        headers: &HeaderMap,
        key: i64,
        team: Option<i64>,
    ) -> Option<Self> {
        if !policy.enabled || policy.validate().is_err() {
            return None;
        }
        // Only text conversations are eligible. Tool definitions and all other
        // parameters remain in the bucket, including unknown Provider options.
        let mut bucket_body = body.clone();
        let obj = bucket_body.as_object_mut()?;
        let mut system = Vec::new();
        let text = if let Some(messages) = obj.remove("messages") {
            let messages = messages.as_array()?;
            let mut last = None;
            let mut conversation = Vec::new();
            for message in messages {
                if message
                    .as_object()?
                    .keys()
                    .any(|key| !matches!(key.as_str(), "role" | "content"))
                {
                    return None;
                }
                let role = message.get("role")?.as_str()?;
                if !matches!(role, "system" | "developer" | "user" | "assistant")
                    || message.get("tool_calls").is_some()
                {
                    return None;
                }
                let content = text_content(message.get("content")?)?;
                if matches!(role, "system" | "developer") {
                    system.push(message.clone());
                }
                if role == "user" {
                    last = Some(content);
                }
                conversation.push(message.clone());
            }
            match policy.input {
                Input::LastUser => {
                    // Only the last user message is embedded. Every other
                    // message (roles, order, any assistant continuation) goes
                    // into the bucket, so a different history never reuses
                    // another caller's answer.
                    let idx = conversation.iter().rposition(|m| m["role"] == "user")?;
                    let mut context = conversation.clone();
                    context.remove(idx);
                    system = context;
                    last?
                }
                Input::Conversation => serde_json::to_string(&conversation).ok()?,
            }
        } else {
            let input = obj.remove("input")?;
            // Responses with structured inputs need a text-only normalization
            // before semantic reuse is safe. Keep exact caching for those.
            input.as_str()?.to_owned()
        };
        let text: String = text.chars().take(policy.max_chars).collect();
        if text.is_empty() {
            return None;
        }
        let system_hash = hex::encode(Sha256::digest(serde_json::to_vec(&system).ok()?));
        let identity = json!([route, policy, system_hash]).to_string();
        let scope = cache::Policy {
            enabled: true,
            ttl_secs: policy.ttl_secs,
            scope: policy.scope,
            ..Default::default()
        };
        let bucket = cache::Plan::new(
            &scope,
            &identity,
            endpoint,
            &bucket_body,
            headers,
            key,
            team,
        )?
        .key;
        let mut exact = cache::Plan::new(&scope, &identity, endpoint, body, headers, key, team)?;
        exact.key = format!("semantic:{}", exact.key);
        Some(Self {
            exact,
            bucket,
            text,
            policy: policy.clone(),
        })
    }
}

fn text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.into());
    }
    let mut text = String::new();
    for block in value.as_array()? {
        if block["type"] != "text"
            || block
                .as_object()?
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "text"))
        {
            return None;
        }
        text.push_str(block.get("text")?.as_str()?);
    }
    Some(text)
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut dot, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (&a, &b) in a.iter().zip(b) {
        if !a.is_finite() || !b.is_finite() {
            return None;
        }
        let (a, b) = (a as f64, b as f64);
        dot += a * b;
        aa += a * a;
        bb += b * b;
    }
    (aa > 0.0 && bb > 0.0).then(|| (dot / (aa.sqrt() * bb.sqrt())).clamp(-1.0, 1.0))
}
pub fn parse_embedding(body: &[u8]) -> Option<Vec<f32>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let raw = value["data"][0]["embedding"].as_array()?;
    if raw.len() > 65_536 {
        return None;
    }
    let vector: Vec<f32> = raw
        .iter()
        .map(|v| v.as_f64().map(|v| v as f32))
        .collect::<Option<_>>()?;
    cosine(&vector, &vector)?;
    Some(vector)
}
pub async fn lookup(
    db: &Db,
    secret: &SecretKey,
    plan: &Plan,
    vector: &[f32],
    limits: &cache::Limits,
) -> Result<Option<(cache::Entry, f64)>> {
    if !plan.exact.lookup || limits.max_entries == 0 || limits.max_bytes == 0 {
        return Ok(None);
    }
    // The LIMIT bounds both database output and cosine work, even before a
    // maintenance pass. The stored vector envelope authenticates row/bucket.
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as("SELECT key,vector FROM response_cache WHERE semantic_bucket=? AND expires_ms>? AND body_bytes<=? AND body_bytes<=? ORDER BY created_ms DESC,rowid DESC LIMIT ?")
        .bind(&plan.bucket).bind(crate::db::now_ms()).bind(limits.max_entry_bytes.min(i64::MAX as u64) as i64).bind(limits.max_bytes.min(i64::MAX as u64) as i64).bind(plan.policy.max_vectors as i64).fetch_all(db).await?;
    let mut best = None;
    let mut score = plan.policy.threshold;
    for (key, blob) in rows {
        let decode = || -> Result<Vec<f32>> {
            let (id, bucket, encoded): (String, String, String) =
                serde_json::from_str(&secret.decrypt(std::str::from_utf8(&blob)?)?)?;
            anyhow::ensure!(
                id == key && bucket == plan.bucket,
                "semantic vector identity mismatch"
            );
            let bytes = STANDARD.decode(encoded)?;
            anyhow::ensure!(
                bytes.len().is_multiple_of(4) && bytes.len() <= 65_536 * 4,
                "invalid semantic vector"
            );
            Ok(bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|v| f32::from_le_bytes(*v))
                .collect())
        };
        if let Ok(stored) = decode()
            && let Some(similarity) = cosine(vector, &stored)
            && similarity >= score
        {
            let mut exact = plan.exact.clone();
            exact.key = key;
            if let Ok(Some(entry)) = cache::lookup(db, secret, &exact, limits).await {
                best = Some((entry, similarity));
                score = similarity;
            }
        }
    }
    Ok(best)
}
pub async fn store(
    db: &Db,
    secret: &SecretKey,
    plan: &WritePlan,
    limits: &cache::Limits,
    entry: &cache::Entry,
) -> Result<()> {
    let bytes: Vec<u8> = plan.vector.iter().flat_map(|n| n.to_le_bytes()).collect();
    let vector = secret
        .encrypt(&serde_json::to_string(&(
            &plan.plan.exact.key,
            &plan.plan.bucket,
            STANDARD.encode(bytes),
        ))?)?
        .into_bytes();
    cache::store_with_vector(
        db,
        secret,
        &plan.plan.exact,
        limits,
        entry,
        Some((&plan.plan.bucket, vector, plan.plan.policy.max_vectors)),
    )
    .await
}
