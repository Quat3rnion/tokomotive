//! Model search for the dashboard model picker.
//!
//! Each [`ModelSource`] gives candidate model IDs. The [`Registry`] asks all
//! sources, merges the hits, removes duplicate IDs, ranks exact and prefix
//! matches first and caps the result. To add a source, add one line to
//! [`Registry::with_builtin`] or call `st.model_sources.register(...)`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde::Serialize;
use sqlx::Row;

use crate::provider::{ProviderAuth, ProviderKind};
use crate::state::AppState;

/// The default number of hits.
pub const DEFAULT_LIMIT: usize = 20;
/// The largest accepted `limit`.
pub const MAX_LIMIT: usize = 50;
/// The longest time a search waits for the async warm-up of all sources.
pub const PREPARE_BUDGET: Duration = Duration::from_secs(3);

/// The context of one search.
pub struct SearchCtx {
    /// Only hits for this Provider (by Provider name).
    pub provider: Option<String>,
    pub state: AppState,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelHit {
    /// The value that goes into the form field.
    pub id: String,
    /// A display name. Equal to `id` when the source has no other name.
    pub label: String,
    pub provider: Option<String>,
    /// "route", "model", "provider" or "models.dev".
    pub source: &'static str,
    /// The context window in tokens.
    pub context: Option<u64>,
    /// A short price text, for example "$2.50 in / $10.00 out per M".
    pub price: Option<String>,
}

impl ModelHit {
    pub fn new(id: impl Into<String>, source: &'static str) -> Self {
        let id = id.into();
        ModelHit {
            label: id.clone(),
            id,
            provider: None,
            source,
            context: None,
            price: None,
        }
    }
}

/// A source of model IDs. `name()` is also the key of the `src=` filter.
pub trait ModelSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// Return at most `limit` candidates for `q`. Must be fast and must not
    /// block: read in-memory data only. The registry ranks the result.
    fn search(&self, ctx: &SearchCtx, q: &str, limit: usize) -> Vec<ModelHit>;
    /// Optional async warm-up (for example a network fetch into a cache).
    /// The registry runs it before `search`, bounded to [`PREPARE_BUDGET`].
    fn prepare(&self, _ctx: &SearchCtx) -> Option<BoxFuture<'static, ()>> {
        None
    }
    /// Like [`ModelSource::prepare`], with the query. For sources that look
    /// up each query in a database. The default calls `prepare`.
    fn prepare_for(&self, ctx: &SearchCtx, _q: &str) -> Option<BoxFuture<'static, ()>> {
        self.prepare(ctx)
    }
}

/// Case-insensitive substring match. An empty query matches all.
pub fn matches(q: &str, text: &str) -> bool {
    q.is_empty() || text.to_lowercase().contains(&q.to_lowercase())
}

/// Lower is better: 0 exact ID, 1 exact label, 2 ID prefix, 3 prefix of a
/// part of the ID (after `/ - . _ :`), 4 contains, 5 other.
pub fn score(q: &str, id: &str, label: &str) -> u8 {
    let q = q.trim().to_lowercase();
    let id = id.to_lowercase();
    let label = label.to_lowercase();
    if q.is_empty() {
        return 4;
    }
    if id == q {
        0
    } else if label == q {
        1
    } else if id.starts_with(&q) || label.starts_with(&q) {
        2
    } else if [&id, &label].into_iter().any(|text| {
        text.split(|c: char| c.is_whitespace() || "/-._:".contains(c))
            .any(|part| part.starts_with(&q))
    }) {
        3
    } else if id.contains(&q) || label.contains(&q) {
        4
    } else {
        5
    }
}

/// Keep the `limit` best matches of one source.
fn best(q: &str, mut hits: Vec<ModelHit>, limit: usize) -> Vec<ModelHit> {
    hits.retain(|h| matches(q, &h.id) || matches(q, &h.label));
    hits.sort_by(|a, b| {
        score(q, &a.id, &a.label)
            .cmp(&score(q, &b.id, &b.label))
            .then(a.provider.cmp(&b.provider))
            .then(a.id.len().cmp(&b.id.len()))
            .then(a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    hits
}

/// Merge the hits of all sources (in source order): remove duplicate IDs
/// (the first keeps its place and gets missing facts from the others), rank
/// and cap.
pub fn merge(q: &str, groups: Vec<Vec<ModelHit>>, limit: usize) -> Vec<ModelHit> {
    let mut out: Vec<(usize, ModelHit)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (order, group) in groups.into_iter().enumerate() {
        for hit in group {
            if let Some(&i) = index.get(&hit.id) {
                let kept = &mut out[i].1;
                kept.context = if kept.source == "provider" && hit.source == "models.dev" {
                    hit.context.or(kept.context)
                } else {
                    kept.context.or(hit.context)
                };
                if kept.price.is_none() {
                    kept.price = hit.price;
                }
                if kept.provider.is_none() {
                    kept.provider = hit.provider;
                }
                continue;
            }
            index.insert(hit.id.clone(), out.len());
            out.push((order, hit));
        }
    }
    out.sort_by(|(oa, a), (ob, b)| {
        let priority = |source| match source {
            "route" => 0,
            "model" => 1,
            _ => 2,
        };
        score(q, &a.id, &a.label)
            .cmp(&score(q, &b.id, &b.label))
            .then(priority(a.source).cmp(&priority(b.source)))
            .then(a.provider.cmp(&b.provider))
            .then(a.id.len().cmp(&b.id.len()))
            .then(a.id.cmp(&b.id))
            .then(oa.cmp(ob))
    });
    out.into_iter().map(|(_, h)| h).take(limit).collect()
}

fn resolved_key(key: &str) -> Option<String> {
    let key = match key.strip_prefix("env:") {
        Some(name) => std::env::var(name).ok()?,
        None => key.to_owned(),
    };
    (!key.trim().is_empty()).then_some(key)
}

/// One credential rule for both model sources. Unknown quota health is usable;
/// exhausted quota is not a sign-out. Providers have no disabled flag.
fn connected(
    p: &crate::state::Provider,
    signin: Option<&crate::oauth::Status>,
    invalid: &HashSet<String>,
) -> bool {
    if p.auth == ProviderAuth::ApiKey {
        p.keys
            .iter()
            .filter_map(|k| resolved_key(k))
            .any(|k| !invalid.contains(&crate::quota::key_id(&k)))
    } else {
        signin.is_some_and(|s| s.connected && s.last_error.is_none()) && !invalid.contains("signin")
    }
}

struct Connections {
    table: Arc<crate::routing::RouterTable>,
    at: Instant,
    providers: Vec<crate::state::Provider>,
}

/// The registered model sources.
pub struct Registry {
    sources: RwLock<Vec<Arc<dyn ModelSource>>>,
    connections: RwLock<Option<Connections>>,
    connection_lock: tokio::sync::Mutex<()>,
}

impl Registry {
    /// The built-in sources. Other modules add one line here.
    pub fn with_builtin() -> Self {
        let sources: Vec<Box<dyn ModelSource>> = vec![
            Box::new(RouteSource),
            Box::new(CatalogSource),
            Box::new(ProviderSource::default()),
            Box::new(crate::models_dev::Source::default()),
        ];
        Registry {
            sources: RwLock::new(sources.into_iter().map(Arc::from).collect()),
            connections: RwLock::new(None),
            connection_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn register(&self, source: Box<dyn ModelSource>) {
        self.sources
            .write()
            .expect("model sources")
            .push(Arc::from(source));
    }

    /// The names of the registered sources, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.sources
            .read()
            .expect("model sources")
            .iter()
            .map(|s| s.name())
            .collect()
    }

    /// Credential and health reads are shared by all sources for five seconds,
    /// and invalidated immediately when the settings snapshot changes.
    async fn prepare_connections(&self, state: &AppState) {
        let _guard = self.connection_lock.lock().await;
        let table = state.router_table();
        if self
            .connections
            .read()
            .expect("model connections")
            .as_ref()
            .is_some_and(|c| {
                Arc::ptr_eq(&c.table, &table) && c.at.elapsed() < Duration::from_secs(5)
            })
        {
            return;
        }
        let mut providers = Vec::new();
        let result: anyhow::Result<()> = async {
            let rows = sqlx::query("SELECT name,keys_json FROM providers")
                .fetch_all(&state.db)
                .await?;
            let keys: HashMap<String, String> = rows
                .into_iter()
                .map(|r| (r.get("name"), r.get("keys_json")))
                .collect();
            let health = sqlx::query(
                "SELECT provider,account FROM provider_health WHERE state='credentials_invalid'",
            )
            .fetch_all(&state.db)
            .await?;
            let mut invalid: HashMap<String, HashSet<String>> = HashMap::new();
            for r in health {
                invalid
                    .entry(r.get("provider"))
                    .or_default()
                    .insert(r.get("account"));
            }
            for p in &table.providers {
                let mut p = p.clone();
                let Some(encoded) = keys.get(&p.name) else {
                    continue;
                };
                let raw: Vec<String> = serde_json::from_str(encoded)?;
                p.keys = Arc::new(
                    raw.into_iter()
                        .filter_map(|k| {
                            let raw = if k.starts_with("env:") {
                                Some(k)
                            } else {
                                state.secret_key.decrypt(&k).ok()
                            }?;
                            resolved_key(&raw)
                        })
                        .collect(),
                );
                let signin = if let Some(flow) = crate::oauth::Flow::from_auth(p.auth) {
                    state.oauth.status(&p.name, flow).await.ok()
                } else {
                    None
                };
                let invalid = invalid.get(&p.name).cloned().unwrap_or_default();
                if connected(&p, signin.as_ref(), &invalid) {
                    p.keys = Arc::new(
                        p.keys
                            .iter()
                            .filter(|k| !invalid.contains(&crate::quota::key_id(k)))
                            .cloned()
                            .collect(),
                    );
                    providers.push(p);
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error=%e, "model picker credentials unavailable");
            providers.clear();
        }
        providers.sort_by(|a, b| a.name.cmp(&b.name));
        *self.connections.write().expect("model connections") = Some(Connections {
            table,
            at: Instant::now(),
            providers,
        });
    }

    pub fn connected_providers(&self) -> Vec<crate::state::Provider> {
        self.connections
            .read()
            .expect("model connections")
            .as_ref()
            .map(|c| c.providers.clone())
            .unwrap_or_default()
    }

    /// A non-selectable picker note, from the same credential snapshot.
    pub fn connection_note(&self, provider: Option<&str>) -> String {
        let connections = self.connections.read().expect("model connections");
        let Some(c) = connections.as_ref() else {
            return String::new();
        };
        if let Some(name) = provider {
            return if c.providers.iter().any(|p| p.name == name) {
                String::new()
            } else {
                "This Provider is not connected. Sign in or add a key.".into()
            };
        }
        let excluded = c.table.providers.len() - c.providers.len();
        match excluded {
            0 => String::new(),
            1 => "1 Provider is not connected".into(),
            n => format!("{n} Providers are not connected"),
        }
    }

    /// Search all sources (or only those named in `only`). `limit` is
    /// clamped to 1..=[`MAX_LIMIT`]. The async warm-up of the sources runs
    /// first and stops after [`PREPARE_BUDGET`].
    pub async fn search(
        &self,
        ctx: &SearchCtx,
        q: &str,
        limit: usize,
        only: Option<&[String]>,
    ) -> Vec<ModelHit> {
        self.prepare_connections(&ctx.state).await;
        if ctx
            .provider
            .as_ref()
            .is_some_and(|name| !self.connected_providers().iter().any(|p| &p.name == name))
        {
            return vec![];
        }
        let limit = limit.clamp(1, MAX_LIMIT);
        let q = q.trim();
        let sources: Vec<Arc<dyn ModelSource>> = self
            .sources
            .read()
            .expect("model sources")
            .iter()
            .filter(|s| only.is_none_or(|o| o.iter().any(|n| n == s.name())))
            .cloned()
            .collect();
        let warm: Vec<_> = sources
            .iter()
            .filter_map(|s| s.prepare_for(ctx, q))
            .collect();
        if !warm.is_empty() {
            let _ =
                tokio::time::timeout(PREPARE_BUDGET, futures_util::future::join_all(warm)).await;
        }
        let groups = sources.iter().map(|s| s.search(ctx, q, limit)).collect();
        merge(q, groups, limit)
    }
}

// ------------------------------------------------------------ built-ins

/// Route names. Not used when the search is scoped to a Provider.
pub struct RouteSource;

impl ModelSource for RouteSource {
    fn name(&self) -> &'static str {
        "route"
    }

    fn search(&self, ctx: &SearchCtx, q: &str, limit: usize) -> Vec<ModelHit> {
        if ctx.provider.is_some() {
            return vec![];
        }
        let table = ctx.state.router_table();
        let hits = table
            .routes
            .iter()
            .filter(|r| matches(q, &r.name))
            .map(|r| {
                let mut h = ModelHit::new(r.name.clone(), "route");
                let n = r.deployments.len();
                h.label = format!("Route with {n} Deployment{}", if n == 1 { "" } else { "s" });
                h
            })
            .collect();
        best(q, hits, limit)
    }
}

/// Catalog Model names and aliases. Scoped: only the names of the Models
/// of that Provider, without aliases.
pub struct CatalogSource;

impl ModelSource for CatalogSource {
    fn name(&self) -> &'static str {
        "model"
    }

    fn search(&self, ctx: &SearchCtx, q: &str, limit: usize) -> Vec<ModelHit> {
        let catalog = ctx.state.catalog();
        let mut hits = Vec::new();
        for m in &catalog.models {
            if ctx.provider.as_ref().is_some_and(|p| p != &m.provider) {
                continue;
            }
            let price = m.cost.as_ref().map(|(_, c)| {
                format!(
                    "${} in / ${} out per M",
                    crate::pricing::fmt_rate(c.base.input),
                    crate::pricing::fmt_rate(c.base.output)
                )
            });
            let hit = |id: &str, label: String| ModelHit {
                id: id.to_owned(),
                label,
                provider: Some(m.provider.clone()),
                source: "model",
                context: None,
                price: price.clone(),
            };
            if matches(q, &m.name) {
                hits.push(hit(&m.name, m.name.clone()));
            }
            // A Provider scope asks for upstream model IDs; aliases are
            // client-side names only.
            if ctx.provider.is_some() {
                continue;
            }
            for a in m.aliases.iter().filter(|a| matches(q, a)) {
                hits.push(hit(a, format!("Alias of {}", m.name)));
            }
        }
        best(q, hits, limit)
    }
}

/// One model from a Provider's model list.
#[derive(Debug, Clone)]
struct Listed {
    id: String,
    label: String,
    context: Option<u64>,
}

#[derive(Clone)]
struct Cached {
    at: Instant,
    ok: bool,
    /// Base URL and key of the fetch; a change makes the entry stale.
    stamp: String,
    models: Arc<Vec<Listed>>,
}

/// The live `GET {base}/v1/models` of API-key Providers. Subscription
/// Providers are never queried. Results are cached per Provider.
pub struct ProviderSource {
    cache: Arc<RwLock<HashMap<String, Cached>>>,
    /// One fetch at a time per Provider.
    locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub ttl: Duration,
    /// How long a failed fetch is remembered.
    pub failure_ttl: Duration,
    pub timeout: Duration,
}

impl Default for ProviderSource {
    fn default() -> Self {
        ProviderSource {
            cache: Arc::default(),
            locks: Arc::default(),
            ttl: Duration::from_secs(600),
            failure_ttl: Duration::from_secs(60),
            timeout: Duration::from_secs(3),
        }
    }
}

/// The most models kept from one Provider list.
const MAX_LISTED: usize = 5000;
/// The largest model list body that is read.
const MAX_BODY: usize = 4 * 1024 * 1024;

fn stamp(p: &crate::state::Provider) -> String {
    use sha2::Digest;
    let key = p.keys.first().map(String::as_str).unwrap_or("");
    let hash = sha2::Sha256::digest(key.as_bytes());
    format!("{}|{}", p.base_url, hex::encode(&hash[..8]))
}

fn parse_list(body: &[u8]) -> Option<Vec<Listed>> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|d| d.as_array())
        .or_else(|| v.as_array())?;
    let mut out = Vec::new();
    for item in items.iter().take(MAX_LISTED) {
        let Some(id) = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 200 && !s.contains(','))
        else {
            continue;
        };
        let label = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .or_else(|| item.get("name"))
            .and_then(|x| x.as_str())
            .filter(|s| s.len() <= 200)
            .unwrap_or(id);
        let context = [
            "context_length",
            "context_window",
            "max_context_length",
            "max_input_tokens",
            "inputTokenLimit",
        ]
        .iter()
        .find_map(|k| item.get(*k).and_then(|x| x.as_u64()));
        out.push(Listed {
            id: id.to_owned(),
            label: label.to_owned(),
            context,
        });
    }
    Some(out)
}

async fn fetch_list(
    state: crate::state::AppState,
    p: crate::state::Provider,
    timeout: Duration,
) -> Option<Vec<Listed>> {
    if matches!(p.kind, ProviderKind::Bedrock | ProviderKind::Cohere) {
        return None;
    }
    let key = p.keys.first()?.clone();
    let deadline = Instant::now() + timeout;
    let first = crate::provider::upstream_url(&p.base_url, p.kind.models_path());
    let second = format!("{}/models", p.base_url.trim_end_matches('/'));
    let mut urls = vec![first];
    if p.kind != ProviderKind::Gemini && urls[0] != second {
        urls.push(second);
    }
    for url in urls {
        let http = state.snapshot_http(&p).await.ok()?;
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let req = p.key_header.apply(http.get(&url).timeout(left), &key);
        let req = if p.kind == ProviderKind::Anthropic {
            req.header("anthropic-version", "2023-06-01")
        } else {
            req
        };
        let Ok(mut resp) = req.send().await else {
            return None;
        };
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !resp.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY {
                return None;
            }
        }
        return parse_list(&body);
    }
    None
}

impl ProviderSource {
    /// The API-key Providers that this search may query.
    fn targets(ctx: &SearchCtx) -> Vec<crate::state::Provider> {
        ctx.state
            .model_sources
            .connected_providers()
            .into_iter()
            .filter(|p| p.auth == ProviderAuth::ApiKey)
            .filter(|p| ctx.provider.as_ref().is_none_or(|n| n == &p.name))
            .take(10)
            .collect()
    }

    fn fresh(&self, p: &crate::state::Provider) -> bool {
        let cache = self.cache.read().expect("provider model cache");
        cache.get(&p.name).is_some_and(|c| {
            let ttl = if c.ok { self.ttl } else { self.failure_ttl };
            c.stamp == stamp(p) && c.at.elapsed() < ttl
        })
    }
}

impl ModelSource for ProviderSource {
    fn name(&self) -> &'static str {
        "provider"
    }

    fn prepare(&self, ctx: &SearchCtx) -> Option<BoxFuture<'static, ()>> {
        let stale: Vec<_> = Self::targets(ctx)
            .into_iter()
            .filter(|p| !self.fresh(p))
            .collect();
        if stale.is_empty() {
            return None;
        }
        let state = ctx.state.clone();
        let cache = self.cache.clone();
        let locks = self.locks.clone();
        let (ttl, failure_ttl, timeout) = (self.ttl, self.failure_ttl, self.timeout);
        Some(Box::pin(async move {
            let jobs = stale.into_iter().map(|p| {
                let (state, cache, locks) = (state.clone(), cache.clone(), locks.clone());
                // The fetch runs in its own task, so a search that stops
                // waiting still fills the cache for the next one.
                let job = tokio::spawn(async move {
                    let lock = locks
                        .lock()
                        .expect("provider model locks")
                        .entry(p.name.clone())
                        .or_default()
                        .clone();
                    let _one = lock.lock().await;
                    let s = stamp(&p);
                    let still_stale = cache
                        .read()
                        .expect("provider model cache")
                        .get(&p.name)
                        .is_none_or(|c| {
                            let t = if c.ok { ttl } else { failure_ttl };
                            c.stamp != s || c.at.elapsed() >= t
                        });
                    if !still_stale {
                        return;
                    }
                    let got = fetch_list(state, p.clone(), timeout).await;
                    if got.is_none() {
                        tracing::debug!(provider = %p.name, "provider model list unavailable");
                    }
                    cache.write().expect("provider model cache").insert(
                        p.name.clone(),
                        Cached {
                            at: Instant::now(),
                            ok: got.is_some(),
                            stamp: s,
                            models: Arc::new(got.unwrap_or_default()),
                        },
                    );
                });
                async move {
                    let _ = job.await;
                }
            });
            futures_util::future::join_all(jobs).await;
        }))
    }

    fn search(&self, ctx: &SearchCtx, q: &str, limit: usize) -> Vec<ModelHit> {
        let targets = Self::targets(ctx);
        let cache = self.cache.read().expect("provider model cache");
        let mut hits = Vec::new();
        for p in &targets {
            let Some(c) = cache.get(&p.name).filter(|c| c.stamp == stamp(p)) else {
                continue;
            };
            for m in c.models.iter() {
                let id = if ctx.provider.is_some() {
                    m.id.clone()
                } else {
                    format!("{}/{}", p.name, m.id)
                };
                if !(matches(q, &id) || matches(q, &m.label)) {
                    continue;
                }
                hits.push(ModelHit {
                    label: if m.label == m.id {
                        id.clone()
                    } else {
                        m.label.clone()
                    },
                    id,
                    provider: Some(p.name.clone()),
                    source: "provider",
                    context: m.context,
                    price: None,
                });
            }
        }
        best(q, hits, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, source: &'static str) -> ModelHit {
        ModelHit::new(id, source)
    }

    #[test]
    fn connected_requires_usable_credentials_and_one_valid_account() {
        let mut p = crate::state::Provider::new("work", ProviderKind::Openai, "", vec![]);
        let mut invalid = HashSet::new();
        assert!(!connected(&p, None, &invalid));
        p.keys = Arc::new(vec!["env:MODEL_PICKER_ABSENT_KEY_17".into(), "  ".into()]);
        assert!(!connected(&p, None, &invalid));
        p.keys = Arc::new(vec!["env:PATH".into()]);
        assert!(connected(&p, None, &invalid));
        p.keys = Arc::new(vec!["key-one".into()]);
        assert!(connected(&p, None, &invalid));
        invalid.insert(crate::quota::key_id("key-one"));
        assert!(!connected(&p, None, &invalid));
        p.keys = Arc::new(vec!["key-one".into(), "key-two".into()]);
        assert!(connected(&p, None, &invalid));
        p.auth = ProviderAuth::Claude;
        assert!(!connected(&p, None, &invalid));
        let mut signin = crate::oauth::Status {
            connected: true,
            ..Default::default()
        };
        assert!(connected(&p, Some(&signin), &invalid));
        signin.last_error = Some("invalid credential".into());
        assert!(!connected(&p, Some(&signin), &invalid));
        signin.last_error = None;
        invalid.insert("signin".into());
        assert!(!connected(&p, Some(&signin), &invalid));
    }

    #[test]
    fn ranks_exact_prefix_word_prefix_substring_then_source_and_provider() {
        let mut word = hit("word", "provider");
        word.label = "The GPT model".into();
        let got = merge(
            "gpt",
            vec![vec![
                hit("agpt", "provider"),
                word,
                hit("gpt-plus", "provider"),
                hit("gpt", "provider"),
            ]],
            20,
        );
        assert_eq!(
            got.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["gpt", "gpt-plus", "word", "agpt"]
        );
        let mut a = hit("a/model", "models.dev");
        a.provider = Some("a".into());
        let mut z = hit("z/model", "provider");
        z.provider = Some("z".into());
        let got = merge(
            "",
            vec![
                vec![z],
                vec![a],
                vec![hit("catalog", "model")],
                vec![hit("route", "route")],
            ],
            20,
        );
        assert_eq!(
            got.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["route", "catalog", "a/model", "z/model"]
        );
    }

    #[test]
    fn ranks_exact_then_prefix_then_contains() {
        let got = merge(
            "gpt-4o",
            vec![vec![
                hit("my-gpt-4o-copy", "route"),
                hit("gpt-4o-mini", "route"),
                hit("gpt-4o", "route"),
            ]],
            20,
        );
        let ids: Vec<_> = got.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["gpt-4o", "gpt-4o-mini", "my-gpt-4o-copy"]);
    }

    #[test]
    fn dedupes_by_id_and_fills_missing_facts() {
        let mut b = hit("gpt-4o", "models.dev");
        b.context = Some(128_000);
        let got = merge("", vec![vec![hit("gpt-4o", "model")], vec![b]], 20);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].source, "model");
        assert_eq!(got[0].context, Some(128_000));
    }

    #[test]
    fn caps_at_limit() {
        let many = (0..80).map(|i| hit(&format!("m{i:02}"), "model")).collect();
        assert_eq!(merge("m", vec![many], 20).len(), 20);
    }

    #[test]
    fn parses_openai_and_anthropic_lists() {
        let a =
            parse_list(br#"{"data":[{"id":"gpt-4o","context_length":128000},{"id":"bad,id"}]}"#)
                .unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].context, Some(128_000));
        let b = parse_list(br#"{"data":[{"id":"claude-x","display_name":"Claude X"}]}"#).unwrap();
        assert_eq!(b[0].label, "Claude X");
        assert!(parse_list(b"not json").is_none());
    }
}
