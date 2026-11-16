//! Ordered request-attribute rules. Deployment identities never depend on row position.
use crate::{db::Db, routing::RouterTable};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conditions {
    pub key_id: Option<i64>,
    pub user_id: Option<i64>,
    pub team_id: Option<i64>,
    pub header_name: String,
    pub header_op: String,
    pub header_value: String,
    pub model: String,
    pub model_op: String,
    pub endpoint: String,
    pub format: String,
    pub stream: Option<bool>,
    pub min_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
    pub has_tools: Option<bool>,
    pub has_images: Option<bool>,
}
pub struct Attributes<'a> {
    pub key: i64,
    pub user: i64,
    pub team: Option<i64>,
    pub headers: &'a HeaderMap,
    pub body: Option<&'a Value>,
    pub model: &'a str,
    pub endpoint: &'a str,
    pub format: &'a str,
}
fn images(value: &Value) -> bool {
    match value {
        Value::Object(o) => {
            matches!(
                o.get("type").and_then(Value::as_str),
                Some("image" | "image_url" | "input_image")
            ) || o.values().any(images)
        }
        Value::Array(a) => a.iter().any(images),
        _ => false,
    }
}
/// The same byte estimate as token counting; generation settings do not count.
pub fn estimated_tokens(body: &Value) -> u64 {
    serde_json::json!({"messages":body.get("messages"),"input":body.get("input"),"system":body.get("system"),"instructions":body.get("instructions"),"tools":body.get("tools"),"prompt":body.get("prompt")}).to_string().len().div_ceil(4) as u64
}
impl Conditions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            [self.key_id, self.user_id, self.team_id]
                .into_iter()
                .flatten()
                .all(|v| v > 0),
            "Choose a valid identity."
        );
        anyhow::ensure!(
            self.min_tokens
                .zip(self.max_tokens)
                .is_none_or(|(min, max)| min <= max),
            "Minimum tokens must not exceed maximum tokens."
        );
        anyhow::ensure!(
            self.model.len() <= 200 && matches!(self.model_op.as_str(), "" | "equals" | "prefix"),
            "Choose Model equals or prefix, with at most 200 characters."
        );
        anyhow::ensure!(
            matches!(
                self.format.as_str(),
                "" | "openai" | "anthropic" | "gemini" | "bedrock"
            ),
            "Choose a client format."
        );
        anyhow::ensure!(
            matches!(
                self.endpoint.as_str(),
                "" | "chat_completions"
                    | "responses"
                    | "messages"
                    | "count_tokens"
                    | "embeddings"
                    | "images"
                    | "rerank"
                    | "audio_transcription"
                    | "audio_translation"
                    | "audio_speech"
                    | "other"
            ),
            "Choose an endpoint."
        );
        if !self.header_name.is_empty() {
            axum::http::HeaderName::from_bytes(self.header_name.as_bytes())
                .map_err(|_| anyhow::anyhow!("Use a valid header name."))?;
            anyhow::ensure!(
                self.header_name.len() <= 128
                    && self.header_value.len() <= 512
                    && matches!(self.header_op.as_str(), "equals" | "contains" | "regex"),
                "Use a header operator and at most 512 bytes for its value."
            );
            if self.header_op == "regex" {
                self.regex()?;
            }
        } else {
            anyhow::ensure!(self.header_value.is_empty(), "Enter the header name.");
        }
        Ok(())
    }
    fn regex(&self) -> anyhow::Result<regex::Regex> {
        regex::RegexBuilder::new(&self.header_value)
            .size_limit(1 << 20)
            .dfa_size_limit(1 << 20)
            .build()
            .map_err(|_| {
                anyhow::anyhow!(
                    "Use a valid header regex of at most 512 bytes and 1 MiB compiled size."
                )
            })
    }
    pub fn matches(&self, a: &Attributes<'_>) -> bool {
        let null = Value::Null;
        let b = a.body.unwrap_or(&null);
        let tools = ["tools", "functions"].iter().any(|k| {
            b.get(k)
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty())
        });
        if self.key_id.is_some_and(|v| v != a.key)
            || self.user_id.is_some_and(|v| v != a.user)
            || self.team_id.is_some_and(|v| Some(v) != a.team)
            || (!self.model.is_empty()
                && if self.model_op == "prefix" {
                    !a.model.starts_with(&self.model)
                } else {
                    a.model != self.model
                })
            || (!self.endpoint.is_empty() && self.endpoint != a.endpoint)
            || (!self.format.is_empty() && self.format != a.format)
            || self
                .stream
                .is_some_and(|v| v != b["stream"].as_bool().unwrap_or(false))
            || self.has_tools.is_some_and(|v| v != tools)
            || self.has_images.is_some_and(|v| v != images(b))
            || self.min_tokens.is_some_and(|v| estimated_tokens(b) < v)
            || self.max_tokens.is_some_and(|v| estimated_tokens(b) > v)
        {
            return false;
        }
        if self.header_name.is_empty() {
            return true;
        }
        a.headers
            .get_all(&self.header_name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.len() <= 4096
                    && match self.header_op.as_str() {
                        "equals" => v == self.header_value,
                        "contains" => v.contains(&self.header_value),
                        "regex" => self.regex().is_ok_and(|r| r.is_match(v)),
                        _ => false,
                    }
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub conditions: Conditions,
    #[serde(default)]
    pub no_fallback: bool,
    #[serde(default)]
    pub canary: Option<Canary>,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canary {
    pub percent_bp: u16,
    pub salt: String,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Matched {
    pub id: String,
    pub name: String,
    pub side: String,
}
pub struct Selection<'a> {
    pub rule: &'a Rule,
    pub action: &'a Action,
    pub side: &'static str,
}
impl Selection<'_> {
    pub fn matched(&self) -> Matched {
        Matched {
            id: self.rule.id.clone(),
            name: self.rule.name.clone(),
            side: self.side.into(),
        }
    }
}
pub fn select<'a>(rules: &'a [Rule], a: &Attributes<'_>) -> Option<Selection<'a>> {
    let rule = rules
        .iter()
        .find(|r| r.enabled && r.conditions.matches(a))?;
    let mut action = &rule.action;
    let mut side = "primary";
    if let Some(canary) = &rule.canary {
        let identity = crate::affinity::conversation_digest(a.headers, a.body)
            .map(hex::encode)
            .unwrap_or_else(|| format!("key:{}", a.key));
        let hash = Sha256::digest(
            serde_json::to_vec(&serde_json::json!([
                "tokomotive-rule-v1",
                rule.id,
                canary.salt,
                identity
            ]))
            .expect("JSON"),
        );
        let bucket = u64::from_be_bytes(hash[..8].try_into().expect("eight bytes")) % 10000;
        if bucket < u64::from(canary.percent_bp) {
            action = &canary.action;
            side = "canary";
        }
    }
    Some(Selection { rule, action, side })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Deployments { deployments: Vec<(String, String)> },
    Route { route: String },
    Reject { message: String },
}
pub async fn load(db: &Db, route: &str) -> anyhow::Result<Vec<Rule>> {
    let raw: Option<String> = sqlx::query_scalar("SELECT rules FROM routes WHERE name=?")
        .bind(route)
        .fetch_optional(db)
        .await?;
    Ok(serde_json::from_str(raw.as_deref().unwrap_or("[]"))?)
}
pub fn apply(table: &RouterTable, route: &str, action: &Action, no_fallback: bool) -> RouterTable {
    let mut table = table.clone();
    let target = if let Action::Route { route } = action {
        table.routes.iter().find(|r| &r.name == route).cloned()
    } else {
        None
    };
    if let Some(r) = table.routes.iter_mut().find(|r| r.name == route) {
        if let Action::Deployments { deployments } = action {
            r.deployments
                .retain(|d| deployments.contains(&(d.provider.clone(), d.model.clone())));
        }
        if let Some(target) = target {
            r.deployments = target.deployments;
            r.strategy = target.strategy;
        }
        if no_fallback {
            r.fallbacks.clear();
        }
    }
    table
}
pub async fn referenced(db: &Db, name: &str) -> anyhow::Result<bool> {
    let all: Vec<String> = sqlx::query_scalar("SELECT rules FROM routes")
        .fetch_all(db)
        .await?;
    for raw in all {
        let rules: Vec<Rule> = serde_json::from_str(&raw)?;
        if rules
            .iter()
            .flat_map(actions)
            .any(|a| matches!(a,Action::Route{route} if route==name))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn validate_deployments(
    rules: &[Rule],
    deployments: &[crate::config::DeploymentCfg],
) -> anyhow::Result<()> {
    for action in rules.iter().flat_map(actions) {
        if let Action::Deployments {
            deployments: chosen,
        } = action
        {
            anyhow::ensure!(
                chosen.iter().all(|(p, m)| deployments
                    .iter()
                    .any(|d| &d.provider == p && &d.model == m)),
                "A rule uses a removed Deployment. Change the rule before you remove the Deployment."
            );
        }
    }
    Ok(())
}
pub async fn validate_graph(db: &Db, current: &str, rules: &[Rule]) -> anyhow::Result<()> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name,rules FROM routes")
        .fetch_all(db)
        .await?;
    let mut graph = std::collections::HashMap::new();
    for (name, raw) in rows {
        let stored: Vec<Rule> = serde_json::from_str(&raw)?;
        let list = if name == current { rules } else { &stored };
        graph.insert(
            name,
            list.iter()
                .flat_map(actions)
                .filter_map(|a| {
                    if let Action::Route { route } = a {
                        Some(route.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
        );
    }
    // Iterative topological check: a long operator-created graph cannot exhaust
    // the request task's stack. Disabled and zero-percent edges count too.
    let mut incoming: std::collections::HashMap<&str, usize> =
        graph.keys().map(|k| (k.as_str(), 0)).collect();
    for edge in graph.values().flatten() {
        let count = incoming
            .get_mut(edge.as_str())
            .ok_or_else(|| anyhow::anyhow!("Choose an existing target Route."))?;
        *count += 1;
    }
    let mut ready: Vec<&str> = incoming
        .iter()
        .filter_map(|(name, count)| (*count == 0).then_some(*name))
        .collect();
    let mut visited = 0;
    while let Some(name) = ready.pop() {
        visited += 1;
        for next in &graph[name] {
            let count = incoming.get_mut(next.as_str()).expect("validated target");
            *count -= 1;
            if *count == 0 {
                ready.push(next);
            }
        }
    }
    anyhow::ensure!(visited == graph.len(), "Route rules must not form a cycle.");
    Ok(())
}
fn actions(r: &Rule) -> impl Iterator<Item = &Action> {
    std::iter::once(&r.action).chain(r.canary.iter().map(|c| &c.action))
}
