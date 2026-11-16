//! Native vendor payloads on the shared bounded event queue and pinned transport.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use crate::{db, state::AppState};

#[derive(Clone, sqlx::FromRow)]
pub struct Exporter {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub site: String,
    pub endpoint: String,
    pub repo_id: String,
    pub spans: bool,
    pub metrics: bool,
    pub enabled: bool,
    pub sample_rate: f64,
    pub include_bodies: bool,
    pub tag_allowlist: String,
    pub batch_size: i64,
    pub interval_ms: i64,
    pub allow_private: bool,
    pub(crate) api_key: String,
}
impl Exporter {
    pub(crate) fn reference_backed(&self) -> bool {
        crate::secrets::is_reference(&self.api_key)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.sample_rate.is_finite() && (0.0..=1.0).contains(&self.sample_rate),
            "Use a sample rate from 0 to 1."
        );
        ensure!(
            (1..=32).contains(&self.batch_size),
            "Use a batch size from 1 to 32."
        );
        ensure!(
            (10..=30000).contains(&self.interval_ms),
            "Use an interval from 10 to 30000 ms."
        );
        ensure!(
            self.tag_allowlist.len() <= 100
                && self.tag_allowlist.split(',').all(|s| matches!(
                    s.trim(),
                    "" | "route" | "model" | "provider" | "team" | "key_name"
                )),
            "Select tags from route, model, provider, team and key_name."
        );
        if self.kind == "datadog" {
            ensure!(
                self.spans || self.metrics,
                "Select spans or metrics for Datadog."
            );
            ensure!(
                self.site != "gov" || !self.spans,
                "Datadog gov supports metrics only, not LLM spans. See https://docs.datadoghq.com/llm_observability/setup/api/ ."
            );
        } else {
            ensure!(
                self.kind == "maxim" && !self.repo_id.is_empty(),
                "Set a Maxim log repository ID."
            );
        }
        ensure!(
            self.repo_id.len() <= 256 && !self.repo_id.chars().any(char::is_control),
            "Use a log repository ID with at most 256 bytes and no control characters."
        );
        let url = reqwest::Url::parse(&self.base_url()?)?;
        ensure!(
            matches!(url.scheme(), "https" | "http")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/"
                && url.as_str().len() <= 8192,
            "Use an HTTP or HTTPS origin without credentials, path, query or fragment."
        );
        Ok(())
    }
    pub(crate) fn blank() -> Self {
        Self {
            id: 0,
            name: String::new(),
            kind: "datadog".into(),
            site: "US1".into(),
            endpoint: String::new(),
            repo_id: String::new(),
            spans: true,
            metrics: false,
            enabled: false,
            sample_rate: 1.0,
            include_bodies: false,
            tag_allowlist: "route,model,provider".into(),
            batch_size: 16,
            interval_ms: 1000,
            allow_private: false,
            api_key: String::new(),
        }
    }
    pub fn base_url(&self) -> Result<String> {
        if self.kind == "maxim" {
            return Ok(if self.endpoint.is_empty() {
                "https://api.getmaxim.ai".into()
            } else {
                self.endpoint.clone()
            });
        }
        let site = match self.site.as_str() {
            "US1" => "datadoghq.com",
            "US3" => "us3.datadoghq.com",
            "US5" => "us5.datadoghq.com",
            "EU1" => "datadoghq.eu",
            "AP1" => "ap1.datadoghq.com",
            "gov" => "ddog-gov.com",
            _ => anyhow::bail!("Select a listed Datadog site."),
        };
        Ok(if self.endpoint.is_empty() {
            format!("https://api.{site}")
        } else {
            self.endpoint.clone()
        })
    }
}
pub async fn list(db: &db::Db) -> Result<Vec<Exporter>> {
    Ok(
        sqlx::query_as("SELECT * FROM exporters ORDER BY id LIMIT 16")
            .fetch_all(db)
            .await?,
    )
}

#[derive(Default)]
pub struct Exporters(RwLock<Arc<Vec<Arc<Destination>>>>);
impl Exporters {
    pub async fn reload(&self, db: &db::Db) -> Result<()> {
        *self.0.write().expect("exporters") = Arc::new(
            list(db)
                .await?
                .into_iter()
                .map(|config| {
                    Arc::new(Destination {
                        config,
                        batch: Mutex::new(None),
                    })
                })
                .collect(),
        );
        Ok(())
    }
    pub(crate) fn snapshot(&self) -> Arc<Vec<Arc<Destination>>> {
        self.0.read().expect("exporters").clone()
    }
}

pub(crate) struct Destination {
    pub config: Exporter,
    batch: Mutex<Option<Arc<Batch>>>,
}
struct Event {
    request: String,
    entry: db::NewEntry,
    bodies: Bodies,
}
#[derive(Default)]
pub(crate) struct Bodies {
    pub input: Option<String>,
    pub output: Option<String>,
}
#[derive(Default)]
struct Batch {
    items: Mutex<Vec<(Event, tokio::sync::OwnedSemaphorePermit)>>,
    ready: tokio::sync::Notify,
}
impl Destination {
    pub(crate) fn enqueue(
        self: &Arc<Self>,
        st: &AppState,
        request: &str,
        entry: &db::NewEntry,
        bodies: Bodies,
    ) {
        if rand::random::<f64>() >= self.config.sample_rate {
            return;
        }
        let Some(slot) = st.hooks.reserve() else {
            metrics::counter!("llm_exporter_dropped_total", "exporter" => self.config.name.clone())
                .increment(1);
            return;
        };
        let mut current = self.batch.lock().expect("export batch");
        let first = current.is_none();
        let batch = current
            .get_or_insert_with(|| Arc::new(Batch::default()))
            .clone();
        let full = {
            let mut items = batch.items.lock().expect("batch items");
            items.push((
                Event {
                    request: request.into(),
                    entry: entry.clone(),
                    bodies,
                },
                slot,
            ));
            items.len() >= self.config.batch_size as usize
        };
        if full {
            current.take();
            batch.ready.notify_one();
        }
        drop(current);
        if first {
            let destination = self.clone();
            let st = st.clone();
            st.clone().spawn_tracked(async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(destination.config.interval_ms as u64)) => {},
                    _ = batch.ready.notified() => {},
                }
                {
                    let mut current = destination.batch.lock().expect("export batch");
                    if current.as_ref().is_some_and(|v| Arc::ptr_eq(v, &batch)) {
                        current.take();
                    }
                }
                let Some(_active) = st.hooks.active().await else { return; };
                let events = std::mem::take(&mut *batch.items.lock().expect("batch items")).into_iter().map(|(e, _)| e).collect::<Vec<_>>();
                let count = events.len() as u64;
                deliver(&st, &destination.config, &events, count).await;
            });
        }
    }
}
#[derive(serde::Serialize)]
pub(crate) struct Delivery {
    status: Option<u16>,
    outcome: &'static str,
}
async fn deliver(st: &AppState, exporter: &Exporter, events: &[Event], count: u64) -> Delivery {
    let result = tokio::time::timeout(Duration::from_secs(45), send(st, exporter, events)).await;
    let status = result.ok().and_then(Result::ok);
    let success = status.is_some_and(|s| (200..300).contains(&s));
    metrics::counter!(if success {"llm_exporter_sent_total"} else {"llm_exporter_failed_total"}, "exporter" => exporter.name.clone()).increment(count);
    if !success {
        metrics::counter!("llm_exporter_dropped_total", "exporter" => exporter.name.clone())
            .increment(count);
    }
    Delivery {
        status,
        outcome: if success { "sent" } else { "failed" },
    }
}
pub(crate) async fn test(st: &AppState, exporter: &Exporter) -> Delivery {
    let Some(slot) = st.hooks.reserve() else {
        metrics::counter!("llm_exporter_dropped_total", "exporter" => exporter.name.clone())
            .increment(1);
        return Delivery {
            status: None,
            outcome: "queue_full",
        };
    };
    let Ok(Some(_active)) = tokio::time::timeout(Duration::from_secs(45), st.hooks.active()).await
    else {
        metrics::counter!("llm_exporter_dropped_total", "exporter" => exporter.name.clone())
            .increment(1);
        return Delivery {
            status: None,
            outcome: "queue_timeout",
        };
    };
    drop(slot);
    let entry = db::NewEntry {
        semantic_cache_hit: false,
        semantic_cache_embed: false,
        route_rule: None,
        cache_hit: false,
        ts_ms: db::now_ms(),
        user_id: 0,
        key_id: 0,
        team_id: None,
        provider: "sample".into(),
        subscription: false,
        endpoint: "exporter_test".into(),
        requested_model: Some("sample".into()),
        model: Some("sample".into()),
        status: 200,
        streamed: false,
        usage: Default::default(),
        reported_cost_micros: None,
        quote: Default::default(),
        estimated: false,
        latency_ms: 1,
        ttft_ms: None,
        client_aborted: false,
        provider_key: None,
        attempts: 0,
        session_affinity: None,
    };
    deliver(
        st,
        exporter,
        &[Event {
            request: crate::hooks::new_secret(),
            entry,
            bodies: Bodies::default(),
        }],
        1,
    )
    .await
}
async fn send(st: &AppState, exporter: &Exporter, events: &[Event]) -> Result<u16> {
    let key = if let Some(env) = exporter.api_key.strip_prefix("env:") {
        crate::config::provider_env_value(env).ok_or_else(|| anyhow::anyhow!("Key unavailable"))?
    } else if crate::secrets::is_secret_ref(&exporter.api_key) {
        let stores = crate::secrets::stores(&st.db).await?;
        let store = crate::secrets::Reference::parse(&exporter.api_key)?.select(&stores)?;
        crate::secrets::resolve(store, &st.secret_key, &st.egress, None, &exporter.api_key)
            .await?
            .expose()
            .to_owned()
    } else {
        st.secret_key.decrypt(&exporter.api_key)?
    };
    let mut header = reqwest::header::HeaderValue::from_str(&key)?;
    header.set_sensitive(true);
    let mut event_tags = Vec::with_capacity(events.len());
    for event in events {
        event_tags.push(tags(st, exporter, &event.entry).await?);
    }
    let auth = if exporter.kind == "maxim" {
        "x-maxim-api-key"
    } else {
        "DD-API-KEY"
    };
    let mut payloads = Vec::new();
    if exporter.kind == "maxim" {
        let logs = events.iter().zip(&event_tags).map(|(event, tags)| {
            let entry = &event.entry;
            let mut trace = json!({"id":event.request,"type":"trace","name":entry.endpoint,
                "tags":tags,
                "metrics":usage_metrics(entry),
                "metadata":{"start_ms":entry.ts_ms,"latency_ms":entry.latency_ms,"status":entry.status,"model":entry.model,"provider":entry.provider}});
            if let Some(input) = &event.bodies.input { trace["input"] = json!(input); }
            if let Some(output) = &event.bodies.output { trace["output"] = json!(output); }
            trace
        }).collect::<Vec<_>>();
        payloads.push((
            "/v1/logs/batch",
            json!({"repoId":exporter.repo_id,"logs":logs}),
        ));
    } else {
        if exporter.spans {
            let spans = events
                .iter()
                .zip(&event_tags)
                .map(|(e, t)| span(e, t))
                .collect::<Vec<_>>();
            payloads.push((
                "/api/intake/llm-obs/v1/trace/spans",
                json!({"data":{"type":"span","attributes":{"ml_app":"tokomotive","spans":spans}}}),
            ));
        }
        if exporter.metrics {
            payloads.push(("/api/v2/series", series(events, &event_tags)));
        }
    }
    let mut status = 0;
    for (path, payload) in payloads {
        status = post(
            st,
            exporter,
            path,
            auth,
            &header,
            &payload,
            events.len() as u64,
        )
        .await?;
        if !(200..300).contains(&status) {
            break;
        }
    }
    Ok(status)
}
async fn post(
    st: &AppState,
    exporter: &Exporter,
    path: &str,
    auth: &str,
    header: &reqwest::header::HeaderValue,
    payload: &Value,
    count: u64,
) -> Result<u16> {
    let body = bytes::Bytes::from(serde_json::to_vec(payload)?);
    ensure!(
        body.len()
            <= if path == "/api/v2/series" {
                512000
            } else {
                5 * 1024 * 1024
            },
        "Export batch too large"
    );
    let url = reqwest::Url::parse(&format!("{}{path}", exporter.base_url()?))?;
    let client =
        crate::secrets::adapters::webhook_client(&url, exporter.allow_private, &st.egress).await?;
    for attempt in 0..3 {
        let response = client
            .post(url.clone())
            .header(auth, header.clone())
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(status.as_u16());
        }
        if attempt == 2 || !(status.as_u16() == 429 || status.is_server_error()) {
            return Ok(status.as_u16());
        }
        metrics::counter!("llm_exporter_failed_total", "exporter" => exporter.name.clone())
            .increment(count);
        let wait = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds.min(2)))
            .unwrap_or_default();
        drop(response);
        tokio::time::sleep(wait.max(Duration::from_millis(100 << attempt))).await;
    }
    unreachable!("bounded attempts")
}
async fn tags(
    st: &AppState,
    exporter: &Exporter,
    entry: &db::NewEntry,
) -> Result<serde_json::Map<String, Value>> {
    let mut tags: serde_json::Map<String, Value> = exporter
        .tag_allowlist
        .split(',')
        .filter_map(|key| {
            let value = match key.trim() {
                "route" => entry.requested_model.as_deref()?,
                "model" => entry.model.as_deref()?,
                "provider" => &entry.provider,
                _ => return None,
            };
            Some((key.trim().into(), json!(value)))
        })
        .collect();
    if exporter
        .tag_allowlist
        .split(',')
        .any(|s| s.trim() == "team")
        && let Some(id) = entry.team_id
        && let Some(name) = sqlx::query_scalar::<_, String>("SELECT name FROM teams WHERE id=?")
            .bind(id)
            .fetch_optional(&st.db)
            .await?
    {
        tags.insert("team".into(), json!(name));
    }
    if exporter
        .tag_allowlist
        .split(',')
        .any(|s| s.trim() == "key_name")
        && let Some(name) =
            sqlx::query_scalar::<_, String>("SELECT label FROM virtual_keys WHERE id=?")
                .bind(entry.key_id)
                .fetch_optional(&st.db)
                .await?
    {
        tags.insert("key_name".into(), json!(name));
    }
    Ok(tags)
}
fn tag_list(tags: &serde_json::Map<String, Value>) -> Vec<String> {
    tags.iter()
        .map(|(k, v)| format!("{k}:{}", v.as_str().unwrap_or_default()))
        .collect()
}
fn series(events: &[Event], tags: &[serde_json::Map<String, Value>]) -> Value {
    let mut groups = std::collections::BTreeMap::<(Vec<String>, i64), [f64; 4]>::new();
    for (event, tags) in events.iter().zip(tags) {
        let entry = &event.entry;
        let values = groups
            .entry((tag_list(tags), entry.ts_ms / 1000))
            .or_default();
        values[0] += 1.0;
        values[1] += entry.usage.total() as f64;
        values[2] += entry.quote.cost_micros as f64 / 1_000_000.0;
        values[3] += entry.latency_ms as f64 / 1000.0;
    }
    let series = groups.into_iter().flat_map(|((tags,ts),values)| {
        ["requests","tokens","cost","latency"].into_iter().enumerate().map(move |(i,name)| {
            json!({"metric":format!("tokomotive.{name}"),"type":if i==3 {3} else {1},"interval":1,
                "points":[{"timestamp":ts,"value":if i==3 {values[i]/values[0]} else {values[i]}}],"tags":tags})
        })
    }).collect::<Vec<_>>();
    json!({"series":series})
}
fn usage_metrics(entry: &db::NewEntry) -> Value {
    json!({"input_tokens":entry.usage.total_input(),"output_tokens":entry.usage.output+entry.usage.image_output,"total_tokens":entry.usage.total(),"total_cost":entry.quote.cost_micros as f64 / 1_000_000.0})
}
fn span(event: &Event, tags: &serde_json::Map<String, Value>) -> Value {
    let request = &event.request;
    let entry = &event.entry;
    let tags = tag_list(tags);
    let id = u64::from_str_radix(&request[..16], 16)
        .unwrap_or(1)
        .max(1)
        .to_string();
    let mut span = json!({
        "parent_id":"undefined","trace_id":id,"span_id":id,"name":entry.endpoint,
        "start_ns":entry.ts_ms.max(0) as u64 * 1_000_000,"duration":entry.latency_ms.max(0) as u64 * 1_000_000,
        "status":if entry.status < 400 {"ok"} else {"error"},
        "meta":{"kind":"llm","model_name":entry.model,"model_provider":entry.provider},
        "metrics":usage_metrics(entry),
        "tags":tags
    });
    if let Some(input) = &event.bodies.input {
        span["meta"]["input"] = json!({"value":input});
    }
    if let Some(output) = &event.bodies.output {
        span["meta"]["output"] = json!({"value":output});
    }
    span
}
