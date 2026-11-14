//! Bounded prompt and response rules. No matched text is written to diagnostics.
use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Result, bail, ensure};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

const PATTERN_LIMIT: usize = 8192;
const PROGRAM_LIMIT: usize = 1 << 20;
const MATCH_LIMIT: usize = 1024;
const DEFAULT_TEXT_LIMIT: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Request,
    Response,
    Both,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Log,
    Redact,
    Block,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Redact => "redact",
            Self::Block => "block",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Regex {
        pattern: String,
    },
    Keywords {
        words: Vec<String>,
        #[serde(default)]
        whole_word: bool,
        #[serde(default)]
        case_sensitive: bool,
    },
    Detector {
        detector: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Rule {
    pub name: String,
    pub kind: Kind,
    pub action: Action,
    pub direction: Direction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RuleSet {
    pub name: String,
    pub rules: Vec<Rule>,
    pub hook_url: String,
    pub fail_closed: bool,
    pub max_text_bytes: usize,
}
impl Default for RuleSet {
    fn default() -> Self {
        Self {
            name: String::new(),
            rules: vec![],
            hook_url: String::new(),
            fail_closed: false,
            max_text_bytes: DEFAULT_TEXT_LIMIT,
        }
    }
}

struct Compiled {
    rule: Rule,
    regex: Regex,
    detector: String,
}
pub struct Engine {
    rules: Vec<Compiled>,
    sets: Vec<RuleSet>,
    limit: usize,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
pub struct Verdict {
    pub text: String,
    pub blocked: Option<String>,
    pub rules: Vec<String>,
    /// Byte offsets in the original text. Matches never contain the matched text.
    pub matches: Vec<Match>,
}
#[derive(Debug, Serialize)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub rule: String,
    pub action: Action,
}

impl Engine {
    pub fn new(sets: Vec<RuleSet>) -> Result<Self> {
        ensure!(sets.len() <= 64, "Too many rule sets.");
        let mut rules = vec![];
        for set in &sets {
            ensure!(
                set.max_text_bytes > 0 && set.max_text_bytes <= MAX_TEXT_LIMIT,
                "Text limit must be 1 to 16777216 bytes."
            );
            ensure!(
                set.rules.len() <= MAX_RULES && rules.len() + set.rules.len() <= 256,
                "Too many rules."
            );
            ensure!(
                valid_hook_url(&set.hook_url),
                "Use an HTTP or HTTPS hook URL without credentials."
            );
            for rule in &set.rules {
                let (regex, detector) = compile_rule(rule)?;
                rules.push(Compiled {
                    rule: rule.clone(),
                    regex,
                    detector,
                });
            }
        }
        let limit = sets
            .iter()
            .map(|s| s.max_text_bytes)
            .min()
            .unwrap_or(DEFAULT_TEXT_LIMIT);
        Ok(Self {
            rules,
            sets,
            limit,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(2))
                .build()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && !self.sets.iter().any(|s| !s.hook_url.is_empty())
    }

    fn applies(&self, direction: Direction) -> bool {
        self.rules
            .iter()
            .any(|r| r.rule.direction == Direction::Both || r.rule.direction == direction)
            || self.sets.iter().any(|s| !s.hook_url.is_empty())
    }

    pub async fn inspect_json(
        &self,
        direction: Direction,
        value: &mut serde_json::Value,
    ) -> Verdict {
        let mut paths = vec![];
        text_paths(value, direction, &mut vec![], &mut paths, false);
        let mut text = String::new();
        let mut ranges = vec![];
        for path in &paths {
            if !ranges.is_empty() {
                text.push('\n');
            }
            let start = text.len();
            text.push_str(at(value, path).as_str().unwrap_or_default());
            ranges.push(start..text.len());
        }
        let verdict = self.inspect(direction, &text).await;
        if verdict.blocked.is_none() {
            for (path, range) in paths.iter().zip(ranges) {
                *at_mut(value, path) =
                    serde_json::Value::String(redact(&text, &verdict.matches, range));
            }
        }
        verdict
    }

    pub async fn inspect(&self, direction: Direction, text: &str) -> Verdict {
        let mut verdict = Verdict {
            text: text.into(),
            blocked: None,
            rules: vec![],
            matches: vec![],
        };
        if !self.applies(direction) {
            return verdict;
        }
        if text.len() > self.limit {
            verdict.blocked = Some("inspection_limit".into());
            verdict.rules.push("inspection_limit".into());
            return verdict;
        }
        let mut names = BTreeSet::new();
        for compiled in &self.rules {
            let rule = &compiled.rule;
            if rule.direction != Direction::Both && rule.direction != direction {
                continue;
            }
            let mut found = false;
            for m in compiled.regex.find_iter(text) {
                if !valid(&compiled.detector, m.as_str()) {
                    continue;
                }
                if verdict.matches.len() >= MATCH_LIMIT {
                    verdict.blocked = Some("match_limit".into());
                    names.insert("match_limit".into());
                    break;
                }
                found = true;
                names.insert(rule.name.clone());
                verdict.matches.push(Match {
                    start: m.start(),
                    end: m.end(),
                    rule: rule.name.clone(),
                    action: rule.action,
                });
                if rule.action == Action::Block {
                    verdict.blocked.get_or_insert(rule.name.clone());
                }
            }
            if found {
                metrics::counter!("llm_guardrail_matches_total", "rule" => rule.name.clone(), "action" => rule.action.name()).increment(1);
            }
        }
        for set in &self.sets {
            if set.hook_url.is_empty() {
                continue;
            }
            let action = match self.hook(set, direction, text).await {
                Ok(action) => action,
                Err(_) if set.fail_closed => Action::Block,
                Err(_) => continue,
            };
            {
                let name = format!("{}:external", set.name);
                names.insert(name.clone());
                metrics::counter!("llm_guardrail_matches_total", "rule" => name.clone(), "action" => action.name()).increment(1);
                if action == Action::Block {
                    verdict.blocked.get_or_insert(name.clone());
                }
                verdict.matches.push(Match {
                    start: 0,
                    end: text.len(),
                    rule: name,
                    action,
                });
            }
        }
        verdict.rules = names.into_iter().collect();
        verdict.text = redact(text, &verdict.matches, 0..text.len());
        verdict
    }

    async fn hook(&self, set: &RuleSet, direction: Direction, text: &str) -> Result<Action> {
        // The timeout covers headers and the complete bounded response body.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut response = self
                .client
                .post(&set.hook_url)
                .json(&serde_json::json!({"text":text,"direction":direction}))
                .send()
                .await?;
            ensure!(
                response.status().is_success(),
                "Hook returned a non-success status."
            );
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    body.len() + chunk.len() <= 8192,
                    "Hook response exceeds limit."
                );
                body.extend_from_slice(&chunk);
            }
            #[derive(Deserialize)]
            struct Reply {
                action: Action,
            }
            Ok(serde_json::from_slice::<Reply>(&body)?.action)
        })
        .await?
    }
}

/// The largest text limit of a rule set, in bytes.
pub const MAX_TEXT_LIMIT: usize = 16 * DEFAULT_TEXT_LIMIT;
/// The largest number of rules in one rule set.
pub const MAX_RULES: usize = 64;

/// An empty hook URL, or an HTTP(S) URL with a host and no credentials.
pub fn valid_hook_url(url: &str) -> bool {
    url.is_empty()
        || reqwest::Url::parse(url).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
        })
}

/// The detectors, in the order that the dashboard lists them.
pub const DETECTORS: [&str; 5] = ["email", "phone", "credit_card", "iban", "secret"];

/// Why a single rule cannot compile. The dashboard maps each case to a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleError {
    Name,
    Keywords,
    UnknownDetector,
    PatternTooLong,
    PatternInvalid,
    MatchesEmpty,
}
impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Name => "Rule names must contain 1 to 100 bytes, without control characters.",
            Self::Keywords => "Add 1 to 256 non-empty keywords.",
            Self::UnknownDetector => "Unknown detector.",
            Self::PatternTooLong => "Pattern exceeds 8192 bytes.",
            Self::PatternInvalid => "Pattern is not valid or exceeds the 1 MiB program limit.",
            Self::MatchesEmpty => "Rules must not match empty text.",
        })
    }
}
impl std::error::Error for RuleError {}

/// Compile one rule with the size limits that the Proxy uses.
pub fn compile_rule(rule: &Rule) -> std::result::Result<(Regex, String), RuleError> {
    if rule.name.is_empty() || rule.name.len() > 100 || rule.name.chars().any(char::is_control) {
        return Err(RuleError::Name);
    }
    let (pattern, detector) = match &rule.kind {
        Kind::Regex { pattern } => (pattern.clone(), String::new()),
        Kind::Keywords {
            words,
            whole_word,
            case_sensitive,
        } => {
            if words.is_empty() || words.len() > 256 || words.iter().any(|w| w.is_empty()) {
                return Err(RuleError::Keywords);
            }
            let p = words
                .iter()
                .map(|w| regex::escape(w))
                .collect::<Vec<_>>()
                .join("|");
            (
                format!(
                    "{}{}(?:{}){}",
                    if *case_sensitive { "" } else { "(?i)" },
                    if *whole_word { r"\b" } else { "" },
                    p,
                    if *whole_word { r"\b" } else { "" }
                ),
                String::new(),
            )
        }
        Kind::Detector { detector } => (
            detector_pattern(detector).map_err(|_| RuleError::UnknownDetector)?,
            detector.clone(),
        ),
    };
    if pattern.len() > PATTERN_LIMIT {
        return Err(RuleError::PatternTooLong);
    }
    let regex = RegexBuilder::new(&pattern)
        .size_limit(PROGRAM_LIMIT)
        .dfa_size_limit(PROGRAM_LIMIT)
        .build()
        .map_err(|_| RuleError::PatternInvalid)?;
    if regex.is_match("") {
        return Err(RuleError::MatchesEmpty);
    }
    Ok((regex, detector))
}

fn detector_pattern(name: &str) -> Result<String> {
    if name == "iban" {
        let groups = [
            (15, "NO"),
            (16, "BE"),
            (18, "DK EE FI FO GL NL SD FK"),
            (19, "MK SI"),
            (20, "AT BA KZ LT LU XK MN"),
            (21, "CH HR LI LV"),
            (22, "BG BH CR DE GB GE IE ME RS VA"),
            (23, "AE GI IL IQ TL SO OM"),
            (24, "AD CZ ES MD PK PL RO SA SE SK TN VG DZ"),
            (25, "PT ST LY"),
            (26, "IS TR"),
            (27, "FR GR IT MC MR SM BI DJ"),
            (28, "AL AZ CY DO GT HU LB BY SV NI"),
            (29, "BR PS QA UA EG"),
            (30, "JO KW MU"),
            (31, "MT SC"),
            (32, "LC"),
            (33, "RU"),
        ];
        return Ok(format!(
            r"\b(?:{})\b",
            groups
                .iter()
                .map(|(len, countries)| format!(
                    r"(?:{})[0-9]{{2}}(?: ?[A-Z0-9]){{{}}}",
                    countries.replace(' ', "|"),
                    len - 4
                ))
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
    Ok(match name {
        "email" => r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b",
        "phone" => r"\+[1-9][0-9]{6,14}\b",
        "credit_card" => r"\b[0-9](?:[ -]?[0-9]){12,18}\b",
        "secret" => r"(?:sk-(?:ant-)?[A-Za-z0-9_-]{20,}|AKIA[A-Z0-9]{16}|ghp_[A-Za-z0-9]{30,}|xox[bp]-[A-Za-z0-9-]{20,}|AIza[A-Za-z0-9_-]{35}|-----BEGIN (?:RSA |EC |OPENSSH |DSA |ENCRYPTED )?PRIVATE KEY-----|eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})",
        _ => bail!("Unknown detector."),
    }.into())
}
fn valid(detector: &str, value: &str) -> bool {
    match detector {
        "credit_card" => {
            let digits: Vec<u32> = value.chars().filter_map(|c| c.to_digit(10)).collect();
            (13..=19).contains(&digits.len())
                && digits.iter().any(|d| *d != 0)
                && digits
                    .iter()
                    .rev()
                    .enumerate()
                    .map(|(i, d)| {
                        let n = if i % 2 == 1 { d * 2 } else { *d };
                        if n > 9 { n - 9 } else { n }
                    })
                    .sum::<u32>()
                    % 10
                    == 0
        }
        "iban" => {
            let compact: String = value.chars().filter(|c| *c != ' ').collect();
            let mut rem = 0u32;
            for c in compact[4..].bytes().chain(compact[..4].bytes()) {
                let n = if c.is_ascii_digit() {
                    (c - b'0') as u32
                } else {
                    (c - b'A' + 10) as u32
                };
                rem = (rem * if n < 10 { 10 } else { 100 } + n) % 97;
            }
            rem == 1
        }
        _ => true,
    }
}

fn redact(text: &str, matches: &[Match], range: Range<usize>) -> String {
    let mut spans: Vec<_> = matches
        .iter()
        .filter(|m| m.action == Action::Redact && m.start < range.end && m.end > range.start)
        .collect();
    spans.sort_by_key(|m| (m.start, std::cmp::Reverse(m.end)));
    let mut out = String::new();
    let mut cursor = range.start;
    for m in spans {
        if m.end <= cursor {
            continue;
        }
        if m.start > cursor {
            out.push_str(&text[cursor..m.start]);
        }
        if m.start >= range.start {
            out.push_str("[REDACTED:");
            out.push_str(&m.rule);
            out.push(']');
        }
        cursor = m.end.min(range.end);
    }
    out.push_str(&text[cursor..range.end]);
    out
}

#[derive(Clone)]
enum Part {
    Key(String),
    Index(usize),
}
fn at<'a>(mut value: &'a serde_json::Value, path: &[Part]) -> &'a serde_json::Value {
    for part in path {
        value = match part {
            Part::Key(k) => &value[k],
            Part::Index(i) => &value[*i],
        };
    }
    value
}
fn at_mut<'a>(mut value: &'a mut serde_json::Value, path: &[Part]) -> &'a mut serde_json::Value {
    for part in path {
        value = match part {
            Part::Key(k) => &mut value[k],
            Part::Index(i) => &mut value[*i],
        };
    }
    value
}
fn text_paths(
    value: &serde_json::Value,
    direction: Direction,
    path: &mut Vec<Part>,
    paths: &mut Vec<Vec<Part>>,
    text: bool,
) {
    match value {
        serde_json::Value::String(_) if text => paths.push(path.clone()),
        serde_json::Value::Array(values) => {
            for (i, value) in values.iter().enumerate() {
                path.push(Part::Index(i));
                text_paths(value, direction, path, paths, text);
                path.pop();
            }
        }
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()).is_some_and(|t| {
                matches!(
                    t,
                    "image"
                        | "image_url"
                        | "input_image"
                        | "input_audio"
                        | "audio"
                        | "file"
                        | "input_file"
                )
            }) {
                return;
            }
            for (key, value) in map {
                let is_text = matches!(
                    key.as_str(),
                    "text" | "content" | "system" | "input" | "output_text" | "refusal"
                ) || (direction == Direction::Response
                    && key == "delta"
                    && value.is_string())
                    || (direction == Direction::Request && key == "output" && value.is_string());
                let container = matches!(
                    key.as_str(),
                    "messages"
                        | "choices"
                        | "message"
                        | "delta"
                        | "output"
                        | "item"
                        | "part"
                        | "response"
                );
                if !is_text && !container {
                    continue;
                }
                path.push(Part::Key(key.clone()));
                text_paths(value, direction, path, paths, is_text);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Load the union once per request. Changes also apply to stored cache entries.
pub async fn load(
    db: &crate::db::Db,
    route: Option<&str>,
    team: Option<i64>,
    key: i64,
) -> Result<Engine> {
    #[derive(sqlx::FromRow)]
    struct Stored {
        name: String,
        rules: String,
        hook_url: String,
        fail_closed: bool,
        max_text_bytes: i64,
    }
    let rows: Vec<Stored> = sqlx::query_as("SELECT DISTINCT s.name,s.rules,s.hook_url,s.fail_closed,s.max_text_bytes FROM guardrail_sets s JOIN guardrail_attachments a ON a.set_id=s.id WHERE a.route=? OR a.team_id=? OR a.key_id=? ORDER BY s.id")
        .bind(route).bind(team).bind(key).fetch_all(db).await?;
    Engine::new(
        rows.into_iter()
            .map(|r| {
                Ok(RuleSet {
                    name: r.name,
                    rules: serde_json::from_str(&r.rules)?,
                    hook_url: r.hook_url,
                    fail_closed: r.fail_closed,
                    max_text_bytes: usize::try_from(r.max_text_bytes)?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    )
}

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct SessionData {
    engine: Option<Arc<Engine>>,
    names: BTreeSet<String>,
    done: bool,
    blocked: bool,
}
/// Shares only rule names with the asynchronous Ledger writer.
#[derive(Clone, Default)]
pub(crate) struct Session {
    data: Arc<Mutex<SessionData>>,
    notify: Arc<tokio::sync::Notify>,
}
pub(crate) struct Completion(Option<Session>);
impl Completion {
    pub fn disarm(&mut self) {
        self.0.take();
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(session) = &self.0 {
            session.complete();
        }
    }
}
impl Session {
    pub fn completion(&self) -> Completion {
        Completion(Some(self.clone()))
    }
    pub fn configure(&self, engine: Engine) {
        if !engine.is_empty() {
            self.data.lock().unwrap().engine = Some(Arc::new(engine));
        }
    }
    fn engine(&self) -> Option<Arc<Engine>> {
        self.data.lock().unwrap().engine.clone()
    }
    fn record(&self, v: &Verdict) {
        let mut data = self.data.lock().unwrap();
        data.names.extend(v.rules.iter().cloned());
        data.blocked |= v.blocked.is_some();
    }
    fn complete(&self) {
        self.data.lock().unwrap().done = true;
        self.notify.notify_waiters();
    }
    pub async fn ledger(&self, db: &crate::db::Db, id: i64) -> Result<()> {
        self.wait().await;
        let names = self
            .data
            .lock()
            .unwrap()
            .names
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        if !names.is_empty() {
            sqlx::query("INSERT INTO guardrail_events(ledger_id,rule_names) VALUES (?,?)")
                .bind(id)
                .bind(names.join(", "))
                .execute(db)
                .await?;
        }
        Ok(())
    }
    pub async fn wait(&self) -> bool {
        loop {
            let notified = self.notify.notified();
            if self.data.lock().unwrap().done {
                break;
            }
            notified.await;
        }
        self.data.lock().unwrap().blocked
    }
    pub async fn request(&self, value: &mut serde_json::Value) -> Option<String> {
        let engine = self.engine()?;
        let verdict = engine.inspect_json(Direction::Request, value).await;
        self.record(&verdict);
        verdict.blocked
    }
    pub async fn response(&self, response: Response, anthropic: bool, responses: bool) -> Response {
        let Some(engine) = self.engine() else {
            self.complete();
            return response;
        };
        if !engine.applies(Direction::Response) {
            self.complete();
            return response;
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if content_type.starts_with("text/event-stream") {
            let (mut parts, body) = response.into_parts();
            parts.headers.remove(header::CONTENT_LENGTH);
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
            let session = self.clone();
            tokio::spawn(async move {
                let result = guard_stream(body, &engine, &session, &tx).await;
                if let Err(name) = result {
                    session.record(&Verdict {
                        text: String::new(),
                        rules: vec![name.clone()],
                        blocked: Some(name.clone()),
                        matches: vec![],
                    });
                    let value = error_value(&name, anthropic);
                    let event = if anthropic {
                        format!("event: error\ndata: {value}\n\n")
                    } else if responses {
                        format!(
                            "event: response.failed\ndata: {}\n\n",
                            serde_json::json!({"type":"response.failed","response":{"status":"failed","error":value["error"]}})
                        )
                    } else {
                        format!("data: {value}\n\ndata: [DONE]\n\n")
                    };
                    let _ = tx.send(Ok(Bytes::from(event))).await;
                }
                session.complete();
            });
            return Response::from_parts(
                parts,
                Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
            );
        }
        if !content_type.starts_with("application/json") {
            self.complete();
            return response;
        }
        let (mut parts, body) = response.into_parts();
        let result = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(mut value) if value.is_object() => {
                    let verdict = engine.inspect_json(Direction::Response, &mut value).await;
                    self.record(&verdict);
                    match verdict.blocked {
                        Some(name) => blocked(&name, anthropic),
                        None => {
                            parts.headers.remove(header::CONTENT_LENGTH);
                            Response::from_parts(parts, Body::from(value.to_string()))
                        }
                    }
                }
                _ => {
                    self.record(&Verdict {
                        text: String::new(),
                        blocked: Some("response_format".into()),
                        rules: vec!["response_format".into()],
                        matches: vec![],
                    });
                    blocked("response_format", anthropic)
                }
            },
            Err(_) => {
                let v = Verdict {
                    text: String::new(),
                    blocked: Some("inspection_limit".into()),
                    rules: vec!["inspection_limit".into()],
                    matches: vec![],
                };
                self.record(&v);
                blocked("inspection_limit", anthropic)
            }
        };
        self.complete();
        result
    }
}

fn error_value(name: &str, anthropic: bool) -> serde_json::Value {
    let mut value = serde_json::json!({"error":{"type":"invalid_request_error","code":"guardrail_blocked","message":format!("Guardrail blocked this request: {name}.")}});
    if anthropic {
        value["type"] = "error".into();
    }
    value
}
pub(crate) fn blocked(name: &str, anthropic: bool) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(error_value(name, anthropic)),
    )
        .into_response()
}

struct Event {
    prefix: String,
    value: Option<serde_json::Value>,
    original: String,
}
impl Event {
    fn parse(raw: String) -> Result<Self, String> {
        let mut prefix = String::new();
        let mut data = String::new();
        for line in raw.lines() {
            if let Some(s) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(s.trim_start());
            } else {
                prefix.push_str(line);
                prefix.push('\n');
            }
        }
        let value: Option<serde_json::Value> = serde_json::from_str(&data).ok();
        if !data.is_empty() && data != "[DONE]" && value.as_ref().is_none_or(|v| !v.is_object()) {
            return Err("stream_format".into());
        }
        Ok(Self {
            prefix,
            value,
            original: raw,
        })
    }
    fn encode(&self) -> String {
        match &self.value {
            Some(v) => format!("{}data: {v}\n\n", self.prefix),
            None => format!("{}\n\n", self.original),
        }
    }
}

// A 32 KiB text window retains 8 KiB between evaluations. Each SSE frame is
// bounded to 128 KiB and the pending wire buffer to 256 KiB. Rules that need
// more than 8 KiB look-behind cannot reliably match across released windows.
async fn guard_stream(
    body: Body,
    engine: &Engine,
    session: &Session,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    let mut stream = body.into_data_stream();
    let mut wire = Vec::new();
    let mut pending = vec![];
    let mut pending_bytes = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "stream_read_error".to_owned())?;
        for byte in chunk {
            wire.push(byte);
            if wire.len() > 128 * 1024 {
                return Err("stream_window_limit".into());
            }
            if wire.ends_with(b"\n\n") || wire.ends_with(b"\r\n\r\n") {
                let raw = String::from_utf8(std::mem::take(&mut wire))
                    .map_err(|_| "stream_encoding_error".to_owned())?;
                pending_bytes += raw.len();
                pending.push(Event::parse(raw.trim_end_matches(['\r', '\n']).into())?);
                if pending_bytes >= 32 * 1024 {
                    inspect_events(&mut pending, engine, session, tx, false).await?;
                    pending_bytes = pending.iter().map(|e| e.encode().len()).sum();
                    if pending_bytes > 256 * 1024 {
                        return Err("stream_window_limit".into());
                    }
                }
            }
        }
    }
    if !wire.is_empty() {
        return Err("stream_incomplete_event".into());
    }
    inspect_events(&mut pending, engine, session, tx, true).await
}

async fn inspect_events(
    events: &mut Vec<Event>,
    engine: &Engine,
    session: &Session,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    final_window: bool,
) -> Result<(), String> {
    let mut text = String::new();
    let mut slots = vec![];
    let mut ends = vec![];
    for (i, event) in events.iter().enumerate() {
        if let Some(value) = &event.value {
            let mut paths = vec![];
            text_paths(value, Direction::Response, &mut vec![], &mut paths, false);
            for path in paths {
                let start = text.len();
                text.push_str(at(value, &path).as_str().unwrap_or_default());
                slots.push((i, path, start..text.len()));
            }
        }
        ends.push(text.len());
    }
    let verdict = engine.inspect(Direction::Response, &text).await;
    session.record(&verdict);
    if let Some(name) = verdict.blocked {
        return Err(name);
    }
    let mut safe_end = if final_window {
        text.len()
    } else {
        text.len().saturating_sub(8192)
    };
    // Do not release a partial redaction: retained events must preserve the
    // original text until the next complete window is inspected.
    for m in &verdict.matches {
        if m.start < safe_end && m.end > safe_end {
            safe_end = safe_end.min(m.start);
        }
    }
    let mut count = if final_window {
        events.len()
    } else {
        ends.iter().take_while(|end| **end <= safe_end).count()
    };
    while !final_window && count > 0 {
        let boundary = ends[count - 1];
        let crossing = verdict
            .matches
            .iter()
            .filter(|m| m.start < boundary && m.end > boundary)
            .map(|m| m.start)
            .min();
        let Some(start) = crossing else {
            break;
        };
        safe_end = safe_end.min(start);
        count = ends.iter().take_while(|end| **end <= safe_end).count();
    }
    for (i, path, range) in slots {
        if i < count {
            *at_mut(events[i].value.as_mut().unwrap(), &path) =
                serde_json::Value::String(redact(&text, &verdict.matches, range));
        }
    }
    // The inner Proxy still drains the upstream if the caller disconnects.
    // Drain this wrapper too, so final usage and guardrail flags are retained.
    for event in events.drain(..count) {
        let _ = tx.send(Ok(Bytes::from(event.encode()))).await;
    }
    Ok(())
}
