//! TOML configuration. The file seeds the database on start; the dashboard
//! edits the database. Provider Keys come only from this file or the
//! environment.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RouterCfg {
    pub failure_threshold: usize,
    pub cooldown_secs: u64,
    pub max_attempts: usize,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown_secs: 30,
            max_attempts: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    #[default]
    Priority,
    RoundRobin,
    Weighted,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCfg {
    pub name: String,
    #[serde(default)]
    pub strategy: Strategy,
    pub deployments: Vec<DeploymentCfg>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
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
    /// When set, `/metrics` needs `Authorization: Bearer <token>`.
    pub metrics_token: Option<String>,
    pub metrics_token_env: Option<String>,
    /// Output tokens assumed for a Budget reservation when the request does
    /// not set a maximum.
    pub reserve_output_tokens: i64,
    pub session_days: i64,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: "127.0.0.1:8787".into(),
            database: "sqlite://llm-accounting.db".into(),
            admin_password: None,
            admin_password_env: Some("LLM_ACCOUNTING_ADMIN_PASSWORD".into()),
            default_markup_percent: 0.0,
            upstream_timeout_secs: 600,
            metrics_token: None,
            metrics_token_env: Some("LLM_ACCOUNTING_METRICS_TOKEN".into()),
            reserve_output_tokens: 4096,
            session_days: 7,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCfg {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// A Provider Key pool. Keys are used in turn.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Environment variables, each holds one key of the pool.
    #[serde(default)]
    pub api_keys_env: Vec<String>,
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
    #[serde(flatten)]
    pub base: RatesCfg,
    #[serde(default)]
    pub tiers: Vec<TierCfg>,
}

// No deny_unknown_fields: serde does not support it with flatten.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceCfg {
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
    pub team: Option<String>,
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub budget_monthly: bool,
    /// Plain Virtual Keys to create for the User. For development only.
    #[serde(default)]
    pub keys: Vec<String>,
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
            base: self.base.to_rates(),
            tiers: tiers(&self.tiers),
        }
    }
}

impl PriceCfg {
    pub fn to_spec(&self) -> Result<PriceSpec> {
        let has_rates = self.base.as_ref().is_some_and(|b| !b.is_zero()) || !self.tiers.is_empty();
        match (self.markup_percent, has_rates) {
            (Some(_), true) => bail!("a price card has markup_percent and rates; set only one"),
            (Some(p), false) => Ok(PriceSpec::MarkupBp(percent_to_bp(p))),
            (None, true) => Ok(PriceSpec::Rates(CardRates {
                base: self.base.clone().unwrap_or_default().to_rates(),
                tiers: tiers(&self.tiers),
            })),
            (None, false) => bail!("a price card needs markup_percent or rates"),
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
        if self.router.failure_threshold == 0 || self.router.max_attempts == 0 {
            bail!("router failure_threshold and max_attempts must be positive");
        }
        let mut names = std::collections::HashSet::new();
        for r in &self.routes {
            if !names.insert(&r.name) {
                bail!("duplicate route {}", r.name);
            }
            for d in &r.deployments {
                if !self.providers.iter().any(|p| p.name == d.provider) {
                    bail!("route {} uses unknown provider {}", r.name, d.provider);
                }
                if d.weight == 0 {
                    bail!("route {} has zero deployment weight", r.name);
                }
            }
        }
        for m in &self.models {
            if !self.providers.iter().any(|p| p.name == m.provider) {
                bail!("model {} uses unknown provider {}", m.name, m.provider);
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
                bail!("user {} uses unknown team {}", u.name, t);
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
        let single = env_or(self.api_key_env.as_deref(), self.api_key.as_deref());
        let mut out: Vec<String> = single.into_iter().collect();
        out.extend(self.api_keys.iter().cloned());
        out.extend(
            self.api_keys_env
                .iter()
                .filter_map(|e| std::env::var(e).ok()),
        );
        out.retain(|k| !k.trim().is_empty());
        let mut seen = std::collections::HashSet::new();
        out.retain(|k| seen.insert(k.clone()));
        out
    }
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
                .contains("duplicate route")
        );
        let mut invalid = cfg.clone();
        invalid.routes[0].deployments[0].provider = "missing".into();
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown provider")
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
