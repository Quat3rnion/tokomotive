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
pub struct Server {
    pub listen: String,
    pub database: String,
    pub admin_password: Option<String>,
    pub admin_password_env: Option<String>,
    /// Markup used when a Model has no Price Card.
    pub default_markup_percent: f64,
    pub upstream_timeout_secs: u64,
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
        self.server
            .admin_password_env
            .as_ref()
            .and_then(|e| std::env::var(e).ok())
            .filter(|s| !s.is_empty())
            .or_else(|| self.server.admin_password.clone())
    }
}

impl ProviderCfg {
    pub fn key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|e| std::env::var(e).ok())
            .filter(|s| !s.is_empty())
            .or_else(|| self.api_key.clone())
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
}
