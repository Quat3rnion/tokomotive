//! Bounded request diagnostics. Bodies and collector credentials stay encrypted.
use crate::{
    db::{self, Db, NewEntry},
    settings::{self, SecretKey},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub endpoint: String,
    pub headers: String,
    pub service_name: String,
    pub sample_ratio: f64,
    pub body_cap: usize,
    pub retention_days: i64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            headers: String::new(),
            service_name: "tokomotive".into(),
            sample_ratio: 1.0,
            body_cap: 65536,
            retention_days: 7,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.enabled || !self.endpoint.is_empty() {
            let u = reqwest::Url::parse(&self.endpoint)?;
            if !matches!(u.scheme(), "http" | "https")
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
            {
                bail!("Use an HTTP or HTTPS collector URL without credentials.");
            }
        }
        if !(0.0..=1.0).contains(&self.sample_ratio)
            || !(1..=1048576).contains(&self.body_cap)
            || !(1..=3650).contains(&self.retention_days)
            || self.service_name.len() > 256
            || self.headers.len() > 16384
        {
            bail!(
                "Use a sample ratio from 0 to 1, body cap from 1 to 1048576 bytes and retention from 1 to 3650 days."
            );
        }
        headers(&self.headers)?;
        Ok(())
    }
}
fn headers(text: &str) -> Result<reqwest::header::HeaderMap> {
    let mut h = reqwest::header::HeaderMap::new();
    for line in text.lines().filter(|s| !s.trim().is_empty()) {
        let Some((k, v)) = line.split_once(':') else {
            bail!("Use one Header: value per line.");
        };
        let k = reqwest::header::HeaderName::from_bytes(k.trim().as_bytes())?;
        if matches!(
            k.as_str(),
            "host" | "content-length" | "content-type" | "transfer-encoding"
        ) {
            bail!("This export header is reserved.");
        }
        h.insert(k, reqwest::header::HeaderValue::from_str(v.trim())?);
    }
    Ok(h)
}
pub async fn load(db: &Db, key: &SecretKey) -> Result<Settings> {
    let Some(raw) = settings::value(db, "observability").await? else {
        return Ok(Settings::default());
    };
    let mut s: Settings = serde_json::from_str(&raw)?;
    if !s.headers.is_empty() {
        s.headers = key.decrypt(&s.headers)?;
    }
    s.validate()?;
    Ok(s)
}
pub async fn save(db: &Db, key: &SecretKey, s: &Settings) -> Result<()> {
    s.validate()?;
    let mut stored = s.clone();
    if !stored.headers.is_empty() {
        stored.headers = key.encrypt(&stored.headers)?;
    }
    settings::put(db, "observability", &serde_json::to_string(&stored)?).await
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Attempt {
    pub attempt_index: i64,
    pub route: String,
    pub deployment: String,
    pub provider: String,
    pub system: String,
    pub provider_key: Option<i64>,
    pub key_last4: Option<String>,
    pub status: Option<i64>,
    pub error_class: String,
    pub skip_reason: String,
    pub started_ms: i64,
    pub latency_ms: i64,
    pub ttft_ms: Option<i64>,
}
pub fn error_class(status: u16) -> &'static str {
    match status {
        200..=299 => "",
        401 | 403 => "credentials_invalid",
        429 => "rate_limited",
        500..=599 => "transient",
        300..=399 => "redirect",
        _ => "invalid_request",
    }
}

pub struct RequestTrace {
    pub attempts: Vec<Attempt>,
    limit: usize,
    capture: Option<Capture>,
}
impl RequestTrace {
    pub async fn new(
        st: &crate::state::AppState,
        route: Option<&str>,
        key: i64,
        request: &[u8],
        limit: usize,
    ) -> Self {
        let enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_keys WHERE id=? AND capture_bodies=1) OR EXISTS(SELECT 1 FROM routes WHERE name=? AND capture_bodies=1)")
            .bind(key).bind(route.unwrap_or_default()).fetch_one(&st.db).await.unwrap_or(false);
        Self {
            attempts: Vec::new(),
            limit: limit.clamp(1, 100),
            capture: enabled.then(|| Capture::new(request, st.observability.settings().body_cap)),
        }
    }
    pub fn begin(&mut self, c: &crate::routing::Candidate, key: Option<usize>) -> Option<usize> {
        if self.attempts.len() >= self.limit {
            // Actual upstream attempts take priority over skipped candidates.
            let i = self
                .attempts
                .iter()
                .position(|a| !a.skip_reason.is_empty())?;
            self.attempts.remove(i);
            for (i, a) in self.attempts.iter_mut().enumerate() {
                a.attempt_index = i as i64 + 1;
            }
        }
        let i = self.attempts.len();
        let system = match c.provider.kind {
            crate::provider::ProviderKind::Openai => "openai",
            crate::provider::ProviderKind::Anthropic => "anthropic",
            crate::provider::ProviderKind::Gemini => {
                if c.provider.auth == crate::provider::ProviderAuth::GoogleVertex {
                    "vertex_ai"
                } else {
                    "gemini"
                }
            }
            crate::provider::ProviderKind::Bedrock => "aws.bedrock",
            crate::provider::ProviderKind::Cohere => "cohere",
        };
        let last4 = key.and_then(|i| {
            let key = &c.provider.keys[i];
            (key.chars().count() > 4).then(|| {
                key.chars()
                    .rev()
                    .take(4)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect()
            })
        });
        self.attempts.push(Attempt {
            attempt_index: i as i64 + 1,
            route: c.route.clone(),
            deployment: c.model.clone().unwrap_or_default(),
            provider: c.provider.name.clone(),
            system: system.into(),
            provider_key: key.map(|k| k as i64),
            key_last4: last4,
            status: None,
            error_class: "transient".into(),
            skip_reason: String::new(),
            started_ms: db::now_ms(),
            latency_ms: 0,
            ttft_ms: None,
        });
        Some(i)
    }
    pub fn skip(&mut self, c: &crate::routing::Candidate, key: Option<usize>, reason: &str) {
        if self.attempts.len() >= self.limit {
            return;
        }
        if let Some(i) = self.begin(c, key) {
            self.attempts[i].skip_reason = reason.into();
            self.attempts[i].error_class = if reason == "circuit_open" {
                "transient"
            } else {
                reason
            }
            .into();
        }
    }
    pub fn result(&mut self, i: Option<usize>, status: Option<u16>, started: Instant) {
        if let Some(a) = i.and_then(|i| self.attempts.get_mut(i)) {
            a.status = status.map(i64::from);
            a.error_class = status.map_or("transient", error_class).into();
            a.latency_ms = started.elapsed().as_millis() as i64;
        }
    }
    pub fn response(&mut self, bytes: &[u8]) {
        if let Some(c) = &mut self.capture {
            c.append(bytes);
        }
    }
    pub fn finish(&mut self, status: u16, ttft: Option<i64>, ts: i64) {
        if let Some(a) = self
            .attempts
            .iter_mut()
            .rev()
            .find(|a| a.skip_reason.is_empty())
        {
            a.latency_ms = (db::now_ms() - a.started_ms).max(a.latency_ms);
            a.ttft_ms = ttft.map(|t| (t - (a.started_ms - ts)).max(0));
            if status >= 400 && a.status.is_some_and(|s| s < 400) {
                a.error_class = error_class(status).into();
            }
        }
    }
    pub async fn store(&self, db: &Db, key: &SecretKey, id: i64, ts: i64) -> Result<()> {
        let mut tx = crate::db::begin_write(db).await?;
        for a in &self.attempts {
            sqlx::query("INSERT INTO request_attempts(ledger_id,attempt_index,route,deployment,provider,system,provider_key,key_last4,status,error_class,skip_reason,started_ms,latency_ms,ttft_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(id).bind(a.attempt_index).bind(&a.route).bind(&a.deployment).bind(&a.provider).bind(&a.system).bind(a.provider_key).bind(&a.key_last4).bind(a.status).bind(&a.error_class).bind(&a.skip_reason).bind(a.started_ms).bind(a.latency_ms).bind(a.ttft_ms).execute(&mut *tx).await?;
        }
        if let Some(c) = &self.capture {
            let request = key.encrypt(&c.request)?;
            let (response, truncated) = clip(&redact(&String::from_utf8_lossy(&c.response)), c.cap);
            let response = key.encrypt(&response)?;
            sqlx::query("INSERT INTO captured_bodies(ledger_id,created_ms,request,response,request_truncated,response_truncated) VALUES(?,?,?,?,?,?)").bind(id).bind(ts).bind(request).bind(response).bind(c.request_truncated).bind(c.response_truncated || truncated).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if self.capture.is_some() {
            metrics::counter!("tokomotive_captured_bodies_total").increment(2);
        }
        Ok(())
    }
}
pub async fn attempts(db: &Db, id: i64) -> Result<Vec<Attempt>> {
    Ok(sqlx::query_as(
        "SELECT * FROM request_attempts WHERE ledger_id=? ORDER BY attempt_index LIMIT 100",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

struct Capture {
    request: String,
    request_truncated: bool,
    response: Vec<u8>,
    response_truncated: bool,
    cap: usize,
}
impl Capture {
    fn new(request: &[u8], cap: usize) -> Self {
        // Redact before clipping: a token that crosses the cap must not leak.
        let input_truncated = request.len() > cap;
        let request = redact(&String::from_utf8_lossy(
            &request[..request.len().min(cap.saturating_add(4096))],
        ));
        let (request, truncated) = clip(&request, cap);
        Self {
            request,
            request_truncated: truncated || input_truncated,
            response: Vec::new(),
            response_truncated: false,
            cap,
        }
    }
    fn append(&mut self, bytes: &[u8]) {
        // Keep a bounded lookahead so redaction sees tokens across stream chunks.
        let room = self
            .cap
            .saturating_add(4096)
            .saturating_sub(self.response.len());
        self.response
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.response_truncated |= self.response.len() > self.cap || bytes.len() > room;
    }
}
fn clip(s: &str, cap: usize) -> (String, bool) {
    let mut n = s.len().min(cap);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    (s[..n].into(), s.len() > cap)
}
pub fn redact(text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)(?:sk-[a-z0-9_-]+|(?:AKIA|ASIA)[A-Z0-9]{16}|bearer\s+[a-z0-9._~+/=-]+|(?:authorization|x-api-key|api_key|api-key|access_token|refresh_token|client_secret)["\s]*[:=]["\s]*[^"\r\n,}]+)"#).expect("redaction regex"));
    re.replace_all(text, "[REDACTED]").into_owned()
}
#[derive(sqlx::FromRow)]
pub struct Bodies {
    pub request: String,
    pub response: String,
    pub request_truncated: bool,
    pub response_truncated: bool,
}
pub async fn bodies(db: &Db, key: &SecretKey, id: i64) -> Result<Option<Bodies>> {
    let b:Option<Bodies>=sqlx::query_as("SELECT request,response,request_truncated,response_truncated FROM captured_bodies WHERE ledger_id=?").bind(id).fetch_optional(db).await?;
    b.map(|mut b| {
        b.request = key.decrypt(&b.request)?;
        b.response = key.decrypt(&b.response)?;
        Ok(b)
    })
    .transpose()
}
/// Delete one bounded batch. The scheduler repeats this operation each minute.
pub async fn purge(db: &Db, retention_days: i64, now_ms: i64) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM captured_bodies WHERE ledger_id IN (SELECT ledger_id FROM captured_bodies WHERE created_ms<? ORDER BY created_ms LIMIT 500)").bind(now_ms-retention_days.clamp(1,3650)*86400000).execute(db).await?.rows_affected())
}
pub fn start_retention(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let state = st.clone();
            st.spawn_tracked(async move {
                if let Err(e) = purge(
                    &state.db,
                    state.observability.settings().retention_days,
                    db::now_ms(),
                )
                .await
                {
                    tracing::warn!(error=%e,"body retention purge failed");
                }
            });
        }
    })
}

struct Export {
    settings: Arc<Settings>,
    resource: Value,
}
pub struct Observer {
    settings: RwLock<Arc<Settings>>,
    tx: tokio::sync::mpsc::Sender<Export>,
    drops: std::sync::atomic::AtomicU64,
}
impl Observer {
    pub fn new(settings: Settings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Export>(128);
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                // A short batch window and a fixed maximum bound memory use.
                let until = tokio::time::sleep(Duration::from_millis(25));
                tokio::pin!(until);
                while batch.len() < 16 {
                    tokio::select! { _=&mut until => break, item=rx.recv() => match item { Some(v)=>batch.push(v),None=>break } }
                }
                // Configuration can change while items wait. Group only identical snapshots.
                while let Some(first) = batch.pop() {
                    let mut resources = vec![first.resource];
                    let mut i = 0;
                    while i < batch.len() {
                        if Arc::ptr_eq(&first.settings, &batch[i].settings) {
                            resources.push(batch.swap_remove(i).resource);
                        } else {
                            i += 1;
                        }
                    }
                    if let Ok(h) = headers(&first.settings.headers) {
                        match client
                            .post(&first.settings.endpoint)
                            .headers(h)
                            .json(&json!({"resourceSpans":resources}))
                            .send()
                            .await
                        {
                            Ok(r) if r.status().is_success() => {}
                            _ => {
                                metrics::counter!("tokomotive_trace_export_failures_total")
                                    .increment(1);
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            settings: RwLock::new(Arc::new(settings)),
            tx,
            drops: std::sync::atomic::AtomicU64::new(0),
        })
    }
    pub fn settings(&self) -> Arc<Settings> {
        self.settings
            .read()
            .expect("observability settings")
            .clone()
    }
    pub fn configure(&self, s: Settings) {
        *self.settings.write().expect("observability settings") = Arc::new(s);
    }
    pub fn drops(&self) -> u64 {
        self.drops.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn export(&self, entry: &NewEntry, trace: &RequestTrace, user: &str, key_name: &str) {
        let s = self.settings();
        if !s.enabled || rand::random::<f64>() >= s.sample_ratio {
            return;
        }
        let resource = otlp(entry, trace, user, key_name, &s.service_name);
        if self
            .tx
            .try_send(Export {
                settings: s,
                resource,
            })
            .is_err()
        {
            self.drops
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            metrics::counter!("tokomotive_trace_export_queue_drops_total").increment(1);
        }
    }
}
fn attr(k: &str, v: impl Into<String>) -> Value {
    json!({"key":k,"value":{"stringValue":v.into()}})
}
fn int_attr(k: &str, v: i64) -> Value {
    json!({"key":k,"value":{"intValue":v.to_string()}})
}
fn otlp(e: &NewEntry, t: &RequestTrace, user: &str, key: &str, service: &str) -> Value {
    let trace = hex::encode(rand::random::<[u8; 16]>());
    let parent = hex::encode(rand::random::<[u8; 8]>());
    let system = t
        .attempts
        .iter()
        .rev()
        .find(|a| a.skip_reason.is_empty())
        .map_or("unknown", |a| a.system.as_str());
    let mut attrs = vec![
        attr("gen_ai.system", system),
        attr(
            "gen_ai.request.model",
            e.requested_model.as_deref().unwrap_or_default(),
        ),
        attr(
            "gen_ai.response.model",
            e.model.as_deref().unwrap_or_default(),
        ),
        int_attr("gen_ai.usage.input_tokens", e.usage.input),
        int_attr("gen_ai.usage.output_tokens", e.usage.output),
        attr("tokomotive.provider", &e.provider),
        attr("tokomotive.user", user),
        int_attr("tokomotive.user.id", e.user_id),
        attr("tokomotive.key.name", key),
        int_attr("tokomotive.key.id", e.key_id),
        int_attr("http.response.status_code", i64::from(e.status)),
    ];
    if let Some(team) = e.team_id {
        attrs.push(int_attr("tokomotive.team.id", team));
    }
    let mut spans = vec![
        json!({"traceId":trace,"spanId":parent,"name":"gen_ai.request","kind":2,"startTimeUnixNano":(e.ts_ms*1000000).to_string(),"endTimeUnixNano":((e.ts_ms+e.latency_ms)*1000000).to_string(),"attributes":attrs,"status":{"code":if e.status>=400 {2}else{1}}}),
    ];
    for a in &t.attempts {
        let mut attrs = vec![
            attr("gen_ai.system", &a.system),
            attr("gen_ai.request.model", &a.deployment),
            attr("tokomotive.provider", &a.provider),
            attr("tokomotive.route", &a.route),
            int_attr("tokomotive.attempt.index", a.attempt_index),
            attr("tokomotive.error_class", &a.error_class),
            attr("tokomotive.skip_reason", &a.skip_reason),
        ];
        if let Some(status) = a.status {
            attrs.push(int_attr("http.response.status_code", status));
        }
        if let Some(key) = a.provider_key {
            attrs.push(int_attr("tokomotive.provider_key.id", key));
        }
        if let Some(last) = &a.key_last4 {
            attrs.push(attr("tokomotive.provider_key.last4", last));
        }
        if let Some(ttft) = a.ttft_ms {
            attrs.push(int_attr("tokomotive.ttft_ms", ttft));
        }
        spans.push(json!({"traceId":trace,"spanId":hex::encode(rand::random::<[u8;8]>()),"parentSpanId":parent,"name":"gen_ai.attempt","kind":3,"startTimeUnixNano":(a.started_ms*1000000).to_string(),"endTimeUnixNano":((a.started_ms+a.latency_ms)*1000000).to_string(),"attributes":attrs,"status":{"code":if a.error_class.is_empty(){1}else{2}}}));
    }
    json!({"resource":{"attributes":[attr("service.name",service)]},"scopeSpans":[{"scope":{"name":"tokomotive"},"spans":spans}]})
}
