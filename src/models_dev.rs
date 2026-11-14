//! Validated models.dev data. SQLite is the query source; no catalog blob is cached.
use std::{collections::BTreeMap, io::Read, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqliteConnection};

use crate::{
    db::Db,
    pricing::{CardRates, ContextTier, Rates},
    settings,
};

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const SNAPSHOT: &[u8] = include_bytes!("../assets/models-dev.json.gz");

fn now() -> i64 {
    crate::db::now_ms().div_euclid(1000)
}

#[derive(Deserialize)]
struct ProviderData {
    id: String,
    name: String,
    #[serde(default)]
    api: String,
    #[serde(default)]
    npm: String,
    #[serde(default)]
    doc: String,
    models: BTreeMap<String, ModelData>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct Limits {
    pub context: Option<i64>,
    pub input: Option<i64>,
    pub output: Option<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct ModelData {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub canonical_model_id: String,
    #[serde(default)]
    pub limit: Limits,
    #[serde(default)]
    pub modalities: Value,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub tool_call: bool,
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub knowledge: String,
    #[serde(default)]
    pub release_date: String,
    #[serde(default)]
    pub status: String,
    pub cost: Option<Value>,
}

fn validate_cost(v: &Value) -> Result<()> {
    match v {
        Value::Number(n) => {
            let n = n.as_f64().context("Invalid price")?;
            ensure!(
                n.is_finite() && (0.0..=1e12).contains(&n),
                "Invalid or negative price"
            );
        }
        Value::Object(o) => {
            for v in o.values() {
                validate_cost(v)?;
            }
        }
        Value::Array(a) => {
            for v in a {
                validate_cost(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn rates(v: &Value, fallback: Rates, anthropic: bool) -> Result<Rates> {
    let rate = |key: &str, default| -> Result<i64> {
        match v.get(key) {
            Some(n) => {
                Ok((n.as_f64().context("Price must be a number")? * 1_000_000.0).round() as i64)
            }
            None => Ok(default),
        }
    };
    let input = rate("input", fallback.input)?;
    let write = rate("cache_write", fallback.cache_write_5m)?;
    Ok(Rates {
        input,
        // Reasoning tokens use the output Token Class, not an extra charge.
        output: rate("reasoning", rate("output", fallback.output)?)?,
        cache_read: rate("cache_read", fallback.cache_read)?,
        cache_write_5m: write,
        cache_write_1h: rate("cache_write_1h", if anthropic { input * 2 } else { write })?,
        ..fallback
    })
}

pub fn cost_card(cost: &Value, anthropic: bool) -> Result<CardRates> {
    validate_cost(cost)?;
    ensure!(cost.is_object(), "Cost must be an object");
    let base = rates(cost, Rates::default(), anthropic)?;
    let mut tiers = BTreeMap::new();
    if let Some(v) = cost.get("context_over_200k") {
        tiers.insert(200_000, rates(v, base, anthropic)?);
    }
    if let Some(list) = cost.get("tiers") {
        for v in list.as_array().context("Cost tiers must be an array")? {
            if v["tier"]["type"] == "context" {
                let size = v["tier"]["size"]
                    .as_i64()
                    .context("Invalid context tier size")?;
                ensure!(size >= 0, "Invalid context tier size");
                tiers.insert(size, rates(v, base, anthropic)?);
            }
        }
    }
    Ok(CardRates {
        batch_multiplier_bp: None,
        base,
        tiers: tiers
            .into_iter()
            .map(|(above_input_tokens, rates)| ContextTier {
                above_input_tokens,
                rates,
            })
            .collect(),
    })
}

/// Validate before replacement. Data, Card Versions and the ETag commit together.
pub async fn import(db: &Db, bytes: &[u8], etag: Option<&str>) -> Result<()> {
    ensure!(bytes.len() <= MAX_BYTES, "Model data exceeds 16 MB");
    let data: BTreeMap<String, ProviderData> =
        serde_json::from_slice(bytes).context("Invalid models.dev payload")?;
    let count: usize = data.values().map(|p| p.models.len()).sum();
    ensure!(count > 0, "Model data is empty");
    for (key, p) in &data {
        ensure!(key == &p.id && !p.id.is_empty(), "Invalid provider ID");
        for (key, m) in &p.models {
            ensure!(key == &m.id && !m.id.is_empty(), "Invalid model ID");
            for n in [m.limit.context, m.limit.input, m.limit.output]
                .into_iter()
                .flatten()
            {
                ensure!(n >= 0, "Negative model limit");
            }
            if let Some(c) = &m.cost {
                cost_card(c, p.npm.contains("anthropic"))?;
            }
        }
    }
    let mut tx = crate::db::begin_write(db, "models_dev_import").await?;
    let old: i64 = sqlx::query_scalar("SELECT count(*) FROM models_dev_models")
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        count as i64 * 5 >= old * 4,
        "Model data drops more than 20% of models; old data kept"
    );
    sqlx::query("DELETE FROM models_dev_costs")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM models_dev_models")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM models_dev_providers")
        .execute(&mut *tx)
        .await?;
    for p in data.values() {
        sqlx::query("INSERT INTO models_dev_providers VALUES(?,?,?,?,?)")
            .bind(&p.id)
            .bind(&p.name)
            .bind(&p.api)
            .bind(&p.npm)
            .bind(safe_url(&p.doc))
            .execute(&mut *tx)
            .await?;
        for m in p.models.values() {
            sqlx::query("INSERT INTO models_dev_models VALUES(?,?,?,?,?,?,?,?)")
                .bind(&p.id)
                .bind(&m.id)
                .bind(&m.name)
                .bind(&m.canonical_model_id)
                .bind(m.limit.context)
                .bind(m.limit.input)
                .bind(m.limit.output)
                .bind(serde_json::to_string(m)?)
                .execute(&mut *tx)
                .await?;
            if let Some(cost) = &m.cost {
                sqlx::query("INSERT INTO models_dev_costs VALUES(?,?,?)")
                    .bind(&p.id)
                    .bind(&m.id)
                    .bind(serde_json::to_string(&cost_card(
                        cost,
                        p.npm.contains("anthropic"),
                    )?)?)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    let source = format!(
        "models.dev {} {}",
        chrono::Utc::now().format("%Y-%m-%d"),
        etag.unwrap_or("bundled snapshot")
    );
    sync_cards(&mut tx, &source).await?;
    for (key, value) in [
        ("models_dev_etag", etag.unwrap_or("")),
        ("models_dev_error", ""),
    ] {
        sqlx::query("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(key).bind(value).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO settings(key,value) VALUES('models_dev_updated',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(now().to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

fn safe_url(url: &str) -> &str {
    if reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "https" | "http")) {
        url
    } else {
        ""
    }
}

pub async fn seed(db: &Db) -> Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM models_dev_models)")
        .fetch_one(db)
        .await?;
    if !exists {
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(SNAPSHOT)
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        import(db, &bytes, None).await?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetch {
    NotModified,
    Data(Vec<u8>, Option<String>),
}

/// Redirects are disabled entirely, including cross-host redirects.
pub async fn fetch(url: &str, etag: Option<&str>) -> Result<Fetch> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    fetch_with_client(&client, url, etag).await
}

async fn fetch_with_client(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
) -> Result<Fetch> {
    let url = reqwest::Url::parse(url)?;
    ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.username().is_empty()
            && url.password().is_none(),
        "Use an HTTP or HTTPS model data URL without credentials"
    );
    let mut request = client.get(url);
    if let Some(etag) = etag.filter(|s| !s.is_empty()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| crate::egress::request_error("models.dev", e))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetch::NotModified);
    }
    ensure!(
        response.status().is_success(),
        "Model data fetch returned {} (redirects are not followed)",
        response.status()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BYTES as u64),
        "Model data exceeds 16 MB"
    );
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|s| s.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| crate::egress::request_error("models.dev", e))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES,
            "Model data exceeds 16 MB"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(Fetch::Data(bytes, etag))
}

pub async fn refresh(st: &crate::state::AppState) -> Result<()> {
    let _guard = st.settings_write.lock().await;
    let result: Result<()> = async {
        let url = settings::value(&st.db, "models_dev_url")
            .await?
            .context("Missing model data URL")?;
        let etag = settings::value(&st.db, "models_dev_etag").await?;
        match fetch_with_client(
            &st.egress.client(None, crate::egress::Purpose::Models)?,
            &url,
            etag.as_deref(),
        )
        .await?
        {
            Fetch::NotModified => {
                let mut tx = crate::db::begin_write(&st.db, "models_dev_sync").await?;
                sync_cards(
                    &mut tx,
                    &format!(
                        "models.dev {} {}",
                        chrono::Utc::now().format("%Y-%m-%d"),
                        etag.as_deref().unwrap_or("")
                    ),
                )
                .await?;
                tx.commit().await?;
                settings::put(&st.db, "models_dev_updated", &now().to_string()).await?;
                settings::put(&st.db, "models_dev_error", "").await?;
            }
            Fetch::Data(bytes, etag) => import(&st.db, &bytes, etag.as_deref()).await?,
        }
        st.reload_catalog().await?;
        let alerts_st = st.clone();
        st.spawn_tracked(async move {
            if let Err(e) = crate::alerts::models_dev(&alerts_st).await {
                tracing::warn!(error=%e,"price alert check failed");
            }
        });
        Ok(())
    }
    .await;
    if let Err(e) = &result {
        settings::put(&st.db, "models_dev_error", &e.to_string()).await?;
    }
    result
}

pub fn start_background(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let enabled = settings::value(&st.db, "models_dev_enabled")
                .await
                .ok()
                .flatten()
                .as_deref()
                != Some("false");
            let updated = settings::value(&st.db, "models_dev_updated")
                .await
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            if enabled
                && now() - updated >= 86400
                && let Err(e) = refresh(&st).await
            {
                tracing::warn!(error=%e, "models.dev sync failed; old data kept");
            }
            tokio::time::sleep(Duration::from_secs(86400)).await;
        }
    })
}

/// Explicit mapping, provider preset, then exact normalized API URL. Model:
/// exact ID, canonical ID, then longest `<id>-` prefix (catalog precedence).
/// The models.dev provider ID for a Tokomotive Provider, from its preset or
/// its base URL. `None` when nothing matches.
pub async fn provider_for(db: &Db, provider: &str) -> Result<Option<String>> {
    let mut conn = db.acquire().await?;
    let row = sqlx::query("SELECT preset,base_url FROM providers WHERE name=?")
        .bind(provider)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    infer_provider(&mut conn, row.get("preset"), row.get("base_url")).await
}

async fn infer_provider(
    conn: &mut SqliteConnection,
    preset: Option<String>,
    base: Option<String>,
) -> Result<Option<String>> {
    // Providers created before presets were persisted (including seed files)
    // can still use a verified preset's identity. Some upstream entries, such
    // as Anthropic, deliberately have no API URL in models.dev.
    let preset: Option<String> = preset.or_else(|| {
        let url = base
            .as_deref()?
            .trim_end_matches('/')
            .trim_end_matches("/v1");
        crate::presets::PRESETS
            .iter()
            .find(|p| {
                !p.base_url.is_empty()
                    && p.base_url.trim_end_matches('/').trim_end_matches("/v1") == url
            })
            .map(|p| p.name.to_owned())
    });
    Ok(sqlx::query_scalar("SELECT id FROM models_dev_providers WHERE id=? OR (api!='' AND ?!='' AND (rtrim(api,'/')=? OR rtrim(api,'/')||'/v1'=? OR rtrim(api,'/')=?||'/v1')) ORDER BY CASE WHEN id=? THEN 0 ELSE 1 END,id LIMIT 1")
            .bind(preset.as_deref().unwrap_or(""))
            .bind(base.as_deref().unwrap_or(""))
            .bind(base.as_deref().unwrap_or("").trim_end_matches('/'))
            .bind(base.as_deref().unwrap_or("").trim_end_matches('/'))
            .bind(base.as_deref().unwrap_or("").trim_end_matches('/'))
            .bind(preset.as_deref().unwrap_or("")).fetch_optional(&mut *conn).await?)
}

async fn mapping(conn: &mut SqliteConnection, id: i64) -> Result<Option<(String, String)>> {
    let row = sqlx::query("SELECT m.name,m.models_dev_provider,m.models_dev_model,p.preset,p.base_url FROM models m LEFT JOIN providers p ON p.name=m.provider WHERE m.id=?").bind(id).fetch_one(&mut *conn).await?;
    let override_provider: Option<String> = row.get("models_dev_provider");
    let provider = match override_provider {
        Some(p) => Some(p),
        None => infer_provider(conn, row.get("preset"), row.get("base_url")).await?,
    };
    let Some(provider) = provider else {
        return Ok(None);
    };
    let name: String = row
        .get::<Option<String>, _>("models_dev_model")
        .unwrap_or_else(|| row.get("name"));
    let model = sqlx::query_scalar("SELECT id FROM models_dev_models WHERE provider=? AND (id=? OR canonical_model_id=? OR (substr(?,1,length(id)+1)=id||'-')) ORDER BY CASE WHEN id=? THEN 0 WHEN canonical_model_id=? THEN 1 ELSE 2 END,length(id) DESC,id LIMIT 1")
        .bind(&provider).bind(&name).bind(&name).bind(&name).bind(&name).bind(&name).fetch_optional(&mut *conn).await?;
    Ok(model.map(|m| (provider, m)))
}

pub async fn mapped(db: &Db, id: i64) -> Result<Option<(String, String)>> {
    let mut conn = db.acquire().await?;
    mapping(&mut conn, id).await
}

async fn sync_cards(conn: &mut SqliteConnection, source: &str) -> Result<()> {
    let rows = sqlx::query("SELECT m.id,p.kind FROM models m JOIN providers p ON p.name=m.provider WHERE m.cost_pinned=0 AND p.auth='api_key' ORDER BY m.id").fetch_all(&mut *conn).await?;
    for row in rows {
        let id: i64 = row.get("id");
        let Some((provider, model)) = mapping(conn, id).await? else {
            continue;
        };
        let spec: Option<String> =
            sqlx::query_scalar("SELECT spec FROM models_dev_costs WHERE provider=? AND model=?")
                .bind(&provider)
                .bind(&model)
                .fetch_optional(&mut *conn)
                .await?;
        let Some(spec) = spec else { continue };
        let mut card: CardRates = serde_json::from_str(&spec)?;
        // Use the actual Provider format for the 1h cache rule.
        if row.get::<String, _>("kind").contains("anthropic") {
            let details: String = sqlx::query_scalar(
                "SELECT details FROM models_dev_models WHERE provider=? AND id=?",
            )
            .bind(provider)
            .bind(model)
            .fetch_one(&mut *conn)
            .await?;
            let details: ModelData = serde_json::from_str(&details)?;
            if let Some(c) = details.cost {
                card = cost_card(&c, true)?;
            }
        }
        let old: Option<String> = sqlx::query_scalar("SELECT spec FROM cards WHERE model_id=? AND kind='cost' AND team_id IS NULL ORDER BY id DESC LIMIT 1").bind(id).fetch_optional(&mut *conn).await?;
        if let Some(old) = &old {
            card.batch_multiplier_bp = serde_json::from_str::<CardRates>(old)?.batch_multiplier_bp;
        }
        let spec = serde_json::to_string(&card)?;
        if old
            .as_ref()
            .is_some_and(|s| serde_json::from_str::<CardRates>(s).ok().as_ref() == Some(&card))
        {
            continue;
        }
        if let Some(old) = &old
            && large_change(&serde_json::from_str(old)?, &card)
        {
            sqlx::query("INSERT INTO models_dev_alerts(model_id,old_spec,new_spec,created_at) VALUES(?,?,?,?)").bind(id).bind(old).bind(&spec).bind(now()).execute(&mut *conn).await?;
        }
        sqlx::query(
            "INSERT INTO cards(model_id,kind,spec,created_at,source) VALUES(?,'cost',?,?,?)",
        )
        .bind(id)
        .bind(spec)
        .bind(now())
        .bind(source)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn large_change(old: &CardRates, new: &CardRates) -> bool {
    let mut thresholds = vec![0];
    thresholds.extend(
        old.tiers
            .iter()
            .chain(&new.tiers)
            .map(|t| t.above_input_tokens.saturating_add(1)),
    );
    thresholds.into_iter().any(|input| {
        let u = crate::usage::Usage {
            input,
            ..Default::default()
        };
        let a = old.rates_for(&u);
        let b = new.rates_for(&u);
        [
            a.input,
            a.output,
            a.cache_read,
            a.cache_write_5m,
            a.cache_write_1h,
            a.image_input,
            a.image_output,
        ]
        .into_iter()
        .zip([
            b.input,
            b.output,
            b.cache_read,
            b.cache_write_5m,
            b.cache_write_1h,
            b.image_input,
            b.image_output,
        ])
        .any(|(a, b)| (a as i128 - b as i128).abs() * 2 > a as i128)
    })
}

pub async fn output_limit(db: &Db, id: i64) -> Result<Option<i64>> {
    let Some((provider, model)) = mapped(db, id).await? else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT output_limit FROM models_dev_models WHERE provider=? AND id=?")
            .bind(provider)
            .bind(model)
            .fetch_one(db)
            .await?,
    )
}

pub async fn reservation_output(db: &Db, model_id: Option<i64>, fallback: i64) -> Result<i64> {
    match model_id {
        Some(id) => Ok(output_limit(db, id).await?.unwrap_or(fallback)),
        None => Ok(fallback),
    }
}

pub struct Details {
    pub provider: String,
    pub model: String,
    pub data: ModelData,
    pub doc: String,
}
pub async fn details(db: &Db, id: i64) -> Result<Option<Details>> {
    let Some((provider, model)) = mapped(db, id).await? else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT m.details,p.doc FROM models_dev_models m JOIN models_dev_providers p ON p.id=m.provider WHERE m.provider=? AND m.id=?").bind(&provider).bind(&model).fetch_one(db).await?;
    Ok(Some(Details {
        provider,
        model,
        data: serde_json::from_str(row.get("details"))?,
        doc: row.get("doc"),
    }))
}

pub struct SearchHit {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub source: String,
    pub context: Option<i64>,
    pub price: Option<String>,
}
/// Picker adapter: query on demand and bound results, never keep all models in RAM.
/// The picker can call `models_dev::search(&st.db, provider, q, limit).await`
/// and convert each `SearchHit` to its `ModelHit`. `provider` is a models.dev ID.
/// This function is independent of the picker trait, which is not on this branch.
pub async fn search(
    db: &Db,
    provider: Option<&str>,
    q: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let rows = sqlx::query("SELECT m.id,m.name,m.provider,m.context,c.spec FROM models_dev_models m LEFT JOIN models_dev_costs c ON c.provider=m.provider AND c.model=m.id WHERE (? IS NULL OR m.provider=?) AND (instr(lower(m.name),lower(?))>0 OR instr(lower(m.id),lower(?))>0) ORDER BY m.name COLLATE NOCASE,m.provider,m.id LIMIT ?")
        .bind(provider).bind(provider).bind(q).bind(q).bind(limit.min(100) as i64).fetch_all(db).await?;
    rows.into_iter()
        .map(|r| {
            Ok(SearchHit {
                id: r.get("id"),
                label: r.get("name"),
                provider: r.get("provider"),
                source: "models.dev".into(),
                context: r.get("context"),
                price: r
                    .get::<Option<String>, _>("spec")
                    .map(|s| {
                        serde_json::from_str::<CardRates>(&s).map(|c| {
                            format!(
                                "${} / ${} per Mtok",
                                crate::pricing::fmt_rate(c.base.input),
                                crate::pricing::fmt_rate(c.base.output)
                            )
                        })
                    })
                    .transpose()?,
            })
        })
        .collect()
}

/// The models.dev source of the model picker. `prepare_for` runs the
/// database query for the query text and keeps the hits; `search` returns
/// them. The cache holds the last 64 queries only.
type PickerKey = (Option<String>, String);
type PickerCache =
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(PickerKey, Vec<SearchHit>)>>>;

#[derive(Default)]
pub struct Source {
    hits: PickerCache,
}

impl Source {
    const KEEP: usize = 64;
    const LIMIT: usize = 50;
}

impl crate::model_search::ModelSource for Source {
    fn name(&self) -> &'static str {
        "models.dev"
    }

    fn prepare_for(
        &self,
        ctx: &crate::model_search::SearchCtx,
        q: &str,
    ) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        let db = ctx.state.db.clone();
        let provider = ctx.provider.clone();
        let q = q.trim().to_owned();
        let hits = self.hits.clone();
        Some(Box::pin(async move {
            // Map the Tokomotive Provider to its models.dev provider ID. A
            // Provider with no match gets no models.dev hits.
            let dev = match &provider {
                Some(p) => match provider_for(&db, p).await {
                    Ok(Some(id)) => Some(id),
                    _ => return,
                },
                None => None,
            };
            let found = search(&db, dev.as_deref(), &q, Self::LIMIT)
                .await
                .unwrap_or_default();
            let mut cache = hits.lock().expect("models.dev picker cache");
            cache.retain(|(k, _)| k != &(provider.clone(), q.clone()));
            cache.push_back(((provider, q), found));
            while cache.len() > Self::KEEP {
                cache.pop_front();
            }
        }))
    }

    fn search(
        &self,
        ctx: &crate::model_search::SearchCtx,
        q: &str,
        limit: usize,
    ) -> Vec<crate::model_search::ModelHit> {
        let key = (ctx.provider.clone(), q.trim().to_owned());
        let cache = self.hits.lock().expect("models.dev picker cache");
        let Some((_, found)) = cache.iter().rev().find(|(k, _)| k == &key) else {
            return Vec::new();
        };
        found
            .iter()
            .take(limit)
            .map(|h| crate::model_search::ModelHit {
                // Scoped to a Provider: the upstream model ID. Unscoped: the
                // provider/model form that the Proxy can route.
                id: if ctx.provider.is_some() {
                    h.id.clone()
                } else {
                    format!("{}/{}", h.provider, h.id)
                },
                label: h.label.clone(),
                provider: ctx.provider.clone().or_else(|| Some(h.provider.clone())),
                source: "models.dev",
                context: h.context.and_then(|c| u64::try_from(c).ok()),
                price: h.price.clone(),
            })
            .collect()
    }
}
