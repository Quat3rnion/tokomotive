//! Optional TOML seed configuration. The dashboard edits the database.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::pricing::{CardRates, ContextTier, PriceSpec, Rates};
use crate::provider::ProviderKind;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub router: RouterCfg,
    #[serde(default)]
    pub routes: Vec<RouteCfg>,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub providers: Vec<ProviderCfg>,
    #[serde(default)]
    pub models: Vec<ModelCfg>,
    #[serde(default)]
    pub teams: Vec<TeamCfg>,
    #[serde(default)]
    pub users: Vec<UserCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RouterCfg {
    pub unknown_model_route: Option<String>,
    pub failure_threshold: usize,
    pub cooldown_secs: u64,
    pub max_attempts: usize,
    pub session_affinity_ttl_secs: u64,
    pub session_affinity_capacity: usize,
    pub quota_exhausted_percent: f64,
    pub adaptive_epsilon: f64,
    pub adaptive_half_life: f64,
    pub adaptive_prior_decay_minutes: f64,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self {
            unknown_model_route: None,
            failure_threshold: 3,
            cooldown_secs: 30,
            max_attempts: 8,
            session_affinity_ttl_secs: 1800,
            session_affinity_capacity: 100_000,
            quota_exhausted_percent: 98.0,
            adaptive_epsilon: 0.05,
            adaptive_half_life: 10.0,
            adaptive_prior_decay_minutes: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    #[default]
    Priority,
    RoundRobin,
    Weighted,
    Quota,
    Adaptive,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyStrategy {
    #[default]
    RoundRobin,
    Quota,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCfg {
    pub name: String,
    #[serde(default)]
    pub cache: crate::cache::Policy,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub session_affinity: bool,
    pub deployments: Vec<DeploymentCfg>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentCfg {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    pub listen: String,
    pub database: String,
    pub admin_password: Option<String>,
    pub admin_password_env: Option<String>,
    /// Markup used when a Model has no Price Card.
    pub default_markup_percent: f64,
    pub upstream_timeout_secs: u64,
    pub provider_defaults: crate::provider_limits::Limits,
    /// When set, `/metrics` needs `Authorization: Bearer <token>`.
    pub metrics_token: Option<String>,
    pub metrics_token_env: Option<String>,
    /// Output tokens assumed for a Budget reservation when the request does
    /// not set a maximum.
    pub reserve_output_tokens: i64,
    pub audio_upload_limit_mib: usize,
    pub file_upload_limit_mib: usize,
    pub session_days: i64,
    pub mode: String,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: "127.0.0.1:8787".into(),
            database: "sqlite://tokomotive.db".into(),
            admin_password: None,
            admin_password_env: Some("TOKOMOTIVE_ADMIN_PASSWORD".into()),
            default_markup_percent: 0.0,
            upstream_timeout_secs: 600,
            provider_defaults: Default::default(),
            metrics_token: None,
            metrics_token_env: Some("TOKOMOTIVE_METRICS_TOKEN".into()),
            reserve_output_tokens: 4096,
            audio_upload_limit_mib: default_audio_upload_limit_mib(),
            file_upload_limit_mib: default_file_upload_limit_mib(),
            session_days: 7,
            mode: "full".into(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCfg {
    #[serde(default)]
    pub limits: crate::provider_limits::Limits,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub priority: Option<i16>,
    pub name: String,
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    pub kind: ProviderKind,
    #[serde(default)]
    pub auth: crate::provider::ProviderAuth,
    pub base_url: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub key_header: Option<crate::provider::KeyHeader>,
    #[serde(default = "crate::provider::default_include_usage")]
    pub include_usage: bool,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// A Provider Key pool. Keys are used in turn.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Environment variables, each holds one key of the pool.
    #[serde(default)]
    pub api_keys_env: Vec<String>,
}
impl std::fmt::Debug for ProviderCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCfg")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("keys", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCfg {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub cost: CardCfg,
    pub price: Option<PriceCfg>,
}

pub fn default_region() -> String {
    "global".into()
}

/// Rates in USD per million tokens.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatesCfg {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write_5m: f64,
    #[serde(default)]
    pub cache_write_1h: f64,
    #[serde(default)]
    pub image_input: f64,
    #[serde(default)]
    pub image_output: f64,
    #[serde(default)]
    pub service_units: f64,
}

// No deny_unknown_fields: serde does not support it with flatten.
#[derive(Debug, Clone, Deserialize)]
pub struct TierCfg {
    pub above_input_tokens: i64,
    #[serde(flatten)]
    pub rates: RatesCfg,
}

// No deny_unknown_fields: serde does not support it with flatten.
#[derive(Debug, Clone, Deserialize)]
pub struct CardCfg {
    pub batch_multiplier_bp: Option<u16>,
    #[serde(flatten)]
    pub base: RatesCfg,
    #[serde(default)]
    pub tiers: Vec<TierCfg>,
}

// No deny_unknown_fields: serde does not support it with flatten.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceCfg {
    pub batch_multiplier_bp: Option<u16>,
    pub markup_percent: Option<f64>,
    #[serde(flatten)]
    pub base: Option<RatesCfg>,
    #[serde(default)]
    pub tiers: Vec<TierCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamCfg {
    pub name: String,
    #[serde(default)]
    pub price_overrides: Vec<TeamPriceCfg>,
}

// No deny_unknown_fields: serde does not support it with flatten.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamPriceCfg {
    pub model: String,
    #[serde(flatten)]
    pub price: PriceCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserCfg {
    pub name: String,
    /// Provider Access for each newly seeded key. Empty allows API-key providers only.
    #[serde(default)]
    pub key_allowed_providers: Vec<String>,
    pub team: Option<String>,
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub budget_monthly: bool,
    /// User-wide requests per minute. Omitted means unlimited; zero blocks requests.
    #[serde(default, deserialize_with = "UserCfg::limit")]
    pub rpm_limit: Option<i64>,
    /// User-wide completed tokens per minute, across all Token Classes.
    #[serde(default, deserialize_with = "UserCfg::limit")]
    pub tpm_limit: Option<i64>,
    /// Requests per minute for each newly seeded Virtual Key of this User.
    #[serde(default, deserialize_with = "UserCfg::limit")]
    pub key_rpm_limit: Option<i64>,
    /// Completed tokens per minute for each newly seeded Virtual Key.
    #[serde(default, deserialize_with = "UserCfg::limit")]
    pub key_tpm_limit: Option<i64>,
    /// Plain Virtual Keys to create for the User. For development only.
    #[serde(default)]
    pub keys: Vec<String>,
}

impl UserCfg {
    fn limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        let limit = Option::<i64>::deserialize(d)?;
        if limit.is_some_and(|v| v < 0) {
            return Err(serde::de::Error::custom(
                "a rate limit must be non-negative",
            ));
        }
        Ok(limit)
    }
}

pub fn default_audio_upload_limit_mib() -> usize {
    25
}
pub fn default_file_upload_limit_mib() -> usize {
    16
}

pub fn usd_to_micros(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

pub fn percent_to_bp(p: f64) -> i64 {
    (p * 100.0).round() as i64
}

impl RatesCfg {
    pub fn to_rates(&self) -> Rates {
        Rates {
            input: usd_to_micros(self.input),
            output: usd_to_micros(self.output),
            cache_read: usd_to_micros(self.cache_read),
            cache_write_5m: usd_to_micros(self.cache_write_5m),
            cache_write_1h: usd_to_micros(self.cache_write_1h),
            image_input: usd_to_micros(self.image_input),
            image_output: usd_to_micros(self.image_output),
            service_units: usd_to_micros(self.service_units),
        }
    }

    fn is_zero(&self) -> bool {
        self.to_rates() == Rates::default()
    }
}

fn tiers(t: &[TierCfg]) -> Vec<ContextTier> {
    t.iter()
        .map(|t| ContextTier {
            above_input_tokens: t.above_input_tokens,
            rates: t.rates.to_rates(),
        })
        .collect()
}

impl CardCfg {
    pub fn to_card(&self) -> CardRates {
        CardRates {
            batch_multiplier_bp: self.batch_multiplier_bp,
            base: self.base.to_rates(),
            tiers: tiers(&self.tiers),
        }
    }
}

impl PriceCfg {
    pub fn to_spec(&self) -> Result<PriceSpec> {
        if self.markup_percent.is_some() && self.batch_multiplier_bp.is_some() {
            bail!(
                "A Markup Price inherits the Cost Card batch multiplier. Set the multiplier on that Cost Card."
            );
        }
        let has_rates = self.base.as_ref().is_some_and(|b| !b.is_zero()) || !self.tiers.is_empty();
        match (self.markup_percent, has_rates) {
            (Some(_), true) => bail!("A Price Card has markup_percent and rates. Set only one."),
            (Some(p), false) => Ok(PriceSpec::MarkupBp(percent_to_bp(p))),
            (None, true) => Ok(PriceSpec::Rates(CardRates {
                batch_multiplier_bp: self.batch_multiplier_bp,
                base: self.base.clone().unwrap_or_default().to_rates(),
                tiers: tiers(&self.tiers),
            })),
            (None, false) => bail!("A Price Card needs markup_percent or rates."),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        self.server.provider_defaults.validate()?;
        if !(1..=63).contains(&self.server.file_upload_limit_mib) {
            bail!("The file upload limit must be whole MiB from 1 to 63.");
        }
        if !(1..=63).contains(&self.server.audio_upload_limit_mib) {
            bail!("The audio upload limit must be whole MiB from 1 to 63.");
        }
        if !matches!(self.server.mode.as_str(), "full" | "simple") {
            bail!("The mode must be full or simple.");
        }
        if self.server.upstream_timeout_secs == 0
            || self.server.session_days <= 0
            || self.server.session_days > 3650
            || self.server.reserve_output_tokens < 0
            || !self.server.default_markup_percent.is_finite()
        {
            bail!(
                "The timeout and session days must be positive (session days at most 3650). Reserved tokens must be 0 or more, and the Markup must be a number."
            );
        }
        let mut providers = std::collections::HashSet::new();
        for p in &self.providers {
            p.limits.validate()?;
            validate_name(&p.name)?;
            if p.default_model.as_ref().is_some_and(|m| {
                m.trim().is_empty() || m.len() > 200 || m.chars().any(char::is_control)
            }) {
                bail!("Default model must have 1 to 200 characters and no control characters.");
            }
            for env in p.api_key_env.iter().chain(&p.api_keys_env) {
                validate_provider_env(env)?;
            }
            use crate::provider::ProviderAuth;
            if p.auth != ProviderAuth::ApiKey {
                let base = match p.auth {
                    ProviderAuth::Chatgpt => "https://api.openai.com",
                    ProviderAuth::Codex => "https://chatgpt.com/backend-api/codex",
                    ProviderAuth::Claude => "https://api.anthropic.com",
                    ProviderAuth::HuggingFace => "https://router.huggingface.co",
                    ProviderAuth::GoogleVertex => p.base_url.as_str(),
                    ProviderAuth::GithubCopilot => "https://api.githubcopilot.com",
                    ProviderAuth::QwenCode => "https://dashscope.aliyuncs.com/compatible-mode/v1",
                    ProviderAuth::KimiCode => "https://api.kimi.com/coding/v1",
                    ProviderAuth::GeminiCli => "https://cloudcode-pa.googleapis.com",
                    _ => p.base_url.as_str(),
                };
                if p.base_url.trim_end_matches('/') != base
                    || p.api_key.is_some()
                    || p.api_key_env.is_some()
                    || !p.api_keys.is_empty()
                    || !p.api_keys_env.is_empty()
                {
                    bail!("A Subscription Provider needs the base URL {base} and no API keys.");
                }
            }
            if p.auth == ProviderAuth::GoogleVertex {
                // Old sign-ins have no project yet. Keep the dashboard usable;
                // the request URL builder refuses an empty project.
                if !p.project_id.is_empty() {
                    crate::vertex::validate_project(&p.project_id)?;
                }
                if p.base_url != crate::vertex::base_url(&p.region)? {
                    bail!("Vertex base URL must match its region");
                }
            }
            if (matches!(p.kind, ProviderKind::Bedrock | ProviderKind::Cohere)
                && p.auth != ProviderAuth::ApiKey)
                || (p.kind == ProviderKind::Gemini
                    && !matches!(
                        p.auth,
                        ProviderAuth::ApiKey | ProviderAuth::GeminiCli | ProviderAuth::GoogleVertex
                    ))
                || (p.auth == ProviderAuth::GeminiCli && p.kind != ProviderKind::Gemini)
                || matches!(
                    (p.auth, p.kind),
                    (
                        ProviderAuth::Chatgpt
                            | ProviderAuth::Codex
                            | ProviderAuth::HuggingFace
                            | ProviderAuth::Entra
                            | ProviderAuth::GoogleVertex,
                        ProviderKind::Anthropic
                    ) | (ProviderAuth::QwenCode, ProviderKind::Anthropic)
                        | (ProviderAuth::Claude, ProviderKind::Openai)
                )
            {
                bail!(
                    "The authentication of Provider {} does not match its API format.",
                    p.name
                );
            }
            if !providers.insert(&p.name) {
                bail!("A Provider named {} already exists.", p.name);
            }
            if p.kind == ProviderKind::Bedrock {
                crate::sigv4::region_from_base(p.base_url.trim_end_matches('/'))
                    .map_err(anyhow::Error::msg)?;
                if p.api_key.is_some()
                    || p.api_key_env.is_some()
                    || !p.api_keys.is_empty()
                    || !p.api_keys_env.is_empty()
                {
                    bail!("Native Bedrock uses encrypted AWS credentials, not Provider Keys.");
                }
            }
            let url = reqwest::Url::parse(&p.base_url).with_context(|| {
                format!(
                    "Provider {} needs an absolute base URL, for example https://api.openai.com.",
                    p.name
                )
            })?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!(
                    "Provider {} needs an HTTP or HTTPS base URL without credentials, query or fragment.",
                    p.name
                );
            }
            if p.auth == ProviderAuth::Entra
                && (url.scheme() != "https"
                    || !url.host_str().is_some_and(|h| {
                        h.ends_with(".openai.azure.com")
                            || h.ends_with(".services.ai.azure.com")
                            || h.ends_with(".cognitiveservices.azure.com")
                    }))
            {
                bail!("Entra needs the HTTPS Azure resource base URL.");
            }
        }
        if self.router.failure_threshold == 0 || self.router.max_attempts == 0 {
            bail!("The failure threshold and the maximum attempts must be positive.");
        }
        if !self.router.adaptive_epsilon.is_finite()
            || !(0.0..=1.0).contains(&self.router.adaptive_epsilon)
            || !self.router.adaptive_half_life.is_finite()
            || self.router.adaptive_half_life <= 0.0
            || !self.router.adaptive_prior_decay_minutes.is_finite()
            || self.router.adaptive_prior_decay_minutes <= 0.0
        {
            bail!(
                "Adaptive epsilon must be from 0 to 1. Half-life and prior decay minutes must be positive finite numbers."
            );
        }
        if !self.router.quota_exhausted_percent.is_finite()
            || !(0.0..=100.0).contains(&self.router.quota_exhausted_percent)
        {
            bail!("The quota exhaustion threshold must be from 0 to 100 percent.");
        }
        if self.router.session_affinity_ttl_secs == 0 || self.router.session_affinity_capacity == 0
        {
            bail!("The session affinity lifetime and capacity must be positive.");
        }
        let mut names = std::collections::HashSet::new();
        if self
            .router
            .unknown_model_route
            .as_ref()
            .is_some_and(|name| !self.routes.iter().any(|r| &r.name == name))
        {
            bail!("Choose an existing Route for unknown model names.");
        }
        for r in &self.routes {
            r.cache.semantic.validate()?;
            validate_name(&r.name)?;
            if !names.insert(&r.name) {
                bail!("A Route named {} already exists.", r.name);
            }
            for d in &r.deployments {
                if d.model.trim().is_empty() {
                    bail!(
                        "Route {} needs an upstream model for each Deployment.",
                        r.name
                    );
                }
                if !self.providers.iter().any(|p| p.name == d.provider) {
                    bail!(
                        "Route {} uses an unknown Provider {}. Add the Provider first.",
                        r.name,
                        d.provider
                    );
                }
                if d.weight == 0 {
                    bail!(
                        "Route {} has a zero deployment weight. Use 1 or more.",
                        r.name
                    );
                }
            }
        }
        for m in &self.models {
            if !self.providers.iter().any(|p| p.name == m.provider) {
                bail!("Model {} uses an unknown Provider {}.", m.name, m.provider);
            }
            if let Some(p) = &m.price {
                p.to_spec().with_context(|| format!("model {}", m.name))?;
            }
        }
        for t in &self.teams {
            for o in &t.price_overrides {
                o.price
                    .to_spec()
                    .with_context(|| format!("team {} model {}", t.name, o.model))?;
            }
        }
        for u in &self.users {
            if let Some(t) = &u.team
                && !self.teams.iter().any(|x| &x.name == t)
            {
                bail!("User {} uses an unknown Team {}.", u.name, t);
            }
        }
        Ok(())
    }

    pub fn admin_password(&self) -> Option<String> {
        env_or(
            self.server.admin_password_env.as_deref(),
            self.server.admin_password.as_deref(),
        )
    }

    pub fn metrics_token(&self) -> Option<String> {
        env_or(
            self.server.metrics_token_env.as_deref(),
            self.server.metrics_token.as_deref(),
        )
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        bail!("Names must contain only letters, digits, dot, underscore or hyphen.");
    }
    Ok(())
}

/// The value of the environment variable when set and not empty, else the
/// value from the file.
fn env_or(env: Option<&str>, file: Option<&str>) -> Option<String> {
    env.and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty())
        .or_else(|| file.map(str::to_owned))
        .filter(|s| !s.is_empty())
}

impl ProviderCfg {
    /// All keys of the pool: the single key first, then `api_keys`, then
    /// `api_keys_env`. Empty values are skipped; duplicates are removed.
    pub fn keys(&self) -> Vec<String> {
        let single = self
            .api_key_env
            .as_deref()
            .and_then(provider_env_value)
            .or_else(|| self.api_key.clone());
        let mut out: Vec<String> = single.into_iter().collect();
        out.extend(self.api_keys.iter().cloned());
        out.extend(
            self.api_keys_env
                .iter()
                .filter_map(|e| provider_env_value(e)),
        );
        out.retain(|k| !k.trim().is_empty());
        let mut seen = std::collections::HashSet::new();
        out.retain(|k| seen.insert(k.clone()));
        out
    }
}

/// Provider references must never expose service or common host credentials.
pub fn validate_provider_env(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "Use env:NAME for an environment variable reference."
    );
    let upper = name.to_ascii_uppercase();
    anyhow::ensure!(
        !["TOKOMOTIVE_", "AWS_", "SSH_", "CI_"]
            .iter()
            .any(|p| upper.starts_with(p))
            && !upper.ends_with("_PASSWORD")
            && !upper.ends_with("_PRIVATE_KEY")
            && ![
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GITHUB_TOKEN",
                "GH_TOKEN",
                "GH_ENTERPRISE_TOKEN",
                "GITLAB_TOKEN",
                "GLAB_TOKEN",
                "DATABASE_URL",
                "REDIS_URL",
                "SECRET_KEY",
                "PRIVATE_KEY"
            ]
            .contains(&upper.as_str()),
        "This environment variable is reserved and cannot be a Provider Key."
    );
    Ok(())
}

pub(crate) fn provider_env_value(name: &str) -> Option<String> {
    validate_provider_env(name).ok()?;
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.models.is_empty());
    }

    #[test]
    fn price_card_needs_exactly_one_form() {
        let both: PriceCfg = toml::from_str("markup_percent = 10\ninput = 1.0").unwrap();
        assert!(both.to_spec().is_err());
        let rates: PriceCfg = toml::from_str("input = 1.5").unwrap();
        assert!(
            matches!(rates.to_spec().unwrap(), PriceSpec::Rates(r) if r.base.input == 1_500_000)
        );
    }

    #[test]
    fn route_validation_rejects_invalid_deployments_and_duplicate_names() {
        let text = r#"
[[providers]]
name = "test"
kind = "openai"
base_url = "http://localhost"
[[routes]]
name = "smart"
deployments = [{ provider = "test", model = "m", weight = 1 }]
fallbacks = ["loaded-from-db-at-request-time"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        let mut invalid = cfg.clone();
        invalid.routes.push(invalid.routes[0].clone());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("A Route named")
        );
        let mut invalid = cfg.clone();
        invalid.routes[0].deployments[0].provider = "missing".into();
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown Provider")
        );
        let mut invalid = cfg;
        invalid.routes[0].deployments[0].weight = 0;
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("zero deployment weight")
        );
    }
}
