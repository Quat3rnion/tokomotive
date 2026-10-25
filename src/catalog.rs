//! In-memory view of Models and their current Card Versions.
//! Rebuilt from the database after each change.

use std::collections::HashMap;

use crate::pricing::{CardRates, PriceSpec};
use crate::usage::Usage;

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub aliases: Vec<String>,
    pub cost: Option<(i64, CardRates)>,
    pub price: Option<(i64, PriceSpec)>,
    pub team_price: HashMap<i64, (i64, PriceSpec)>,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub models: Vec<ModelEntry>,
    pub default_markup_bp: i64,
}

/// The computed amounts for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quote {
    pub model_id: Option<i64>,
    /// The name of the matched Model (not the dated name from the response).
    pub model_name: Option<String>,
    pub cost_micros: i64,
    pub price_micros: i64,
    pub cost_card_id: Option<i64>,
    pub price_card_id: Option<i64>,
    pub unpriced: bool,
}

impl Catalog {
    /// Find the Model for a model string: exact name, then alias, then the
    /// longest name such that the string starts with `<name>-`.
    pub fn resolve(&self, model: &str) -> Option<&ModelEntry> {
        if let Some(m) = self.models.iter().find(|m| m.name == model) {
            return Some(m);
        }
        if let Some(m) = self
            .models
            .iter()
            .find(|m| m.aliases.iter().any(|a| a == model))
        {
            return Some(m);
        }
        self.models
            .iter()
            .filter(|m| {
                model.len() > m.name.len()
                    && model.starts_with(&m.name)
                    && model.as_bytes()[m.name.len()] == b'-'
            })
            .max_by_key(|m| m.name.len())
    }

    /// Price a request. Precedence for the Price Card:
    /// Team override for the Model, then the Model Price Card, then the Cost
    /// Card with the default Markup.
    pub fn quote(&self, model: Option<&str>, team_id: Option<i64>, usage: &Usage) -> Quote {
        let Some(entry) = model.and_then(|m| self.resolve(m)) else {
            return Quote {
                unpriced: true,
                ..Default::default()
            };
        };
        let Some((cost_id, cost)) = &entry.cost else {
            return Quote {
                model_id: Some(entry.id),
                model_name: Some(entry.name.clone()),
                unpriced: true,
                ..Default::default()
            };
        };
        let team = team_id.and_then(|t| entry.team_price.get(&t));
        let (price_card_id, price_rates) = match team.or(entry.price.as_ref()) {
            Some((id, spec)) => (Some(*id), spec.resolve(cost)),
            None => (None, cost.marked_up(self.default_markup_bp)),
        };
        Quote {
            model_id: Some(entry.id),
            model_name: Some(entry.name.clone()),
            cost_micros: cost.amount(usage),
            price_micros: price_rates.amount(usage),
            cost_card_id: Some(*cost_id),
            price_card_id,
            unpriced: false,
        }
    }

    pub fn provider_for(&self, model: &str) -> Option<&str> {
        self.resolve(model).map(|m| m.provider.as_str())
    }

    /// Whether a model string is allowed by a Virtual Key's Model list.
    pub fn allowed(&self, allowed: &[String], model: Option<&str>) -> bool {
        if allowed.is_empty() {
            return true;
        }
        let Some(model) = model else { return false };
        let canonical = self.resolve(model).map(|m| m.name.as_str());
        allowed
            .iter()
            .any(|a| a == model || Some(a.as_str()) == canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Rates;

    fn entry(id: i64, name: &str) -> ModelEntry {
        ModelEntry {
            id,
            name: name.into(),
            provider: "openai".into(),
            aliases: vec![],
            cost: Some((
                id * 10,
                CardRates {
                    base: Rates {
                        input: 1_000_000,
                        ..Default::default()
                    },
                    tiers: vec![],
                },
            )),
            price: None,
            team_price: HashMap::new(),
        }
    }

    fn cat() -> Catalog {
        let mut mini = entry(2, "gpt-4o-mini");
        mini.price = Some((99, PriceSpec::MarkupBp(10_000)));
        mini.team_price.insert(7, (77, PriceSpec::MarkupBp(5_000)));
        let mut a = entry(3, "claude-x");
        a.aliases = vec!["claude-latest".into()];
        Catalog {
            models: vec![entry(1, "gpt-4o"), mini, a],
            default_markup_bp: 2_000,
        }
    }

    #[test]
    fn resolves_dated_names_to_longest_prefix() {
        let c = cat();
        assert_eq!(c.resolve("gpt-4o-2024-08-06").unwrap().name, "gpt-4o");
        assert_eq!(
            c.resolve("gpt-4o-mini-2024-07-18").unwrap().name,
            "gpt-4o-mini"
        );
        assert_eq!(c.resolve("claude-latest").unwrap().name, "claude-x");
        assert!(c.resolve("gpt-4").is_none());
        assert!(c.resolve("gpt-4oxyz").is_none());
    }

    #[test]
    fn price_precedence() {
        let c = cat();
        let u = Usage {
            input: 1_000_000,
            ..Default::default()
        };
        // default markup 20%
        let q = c.quote(Some("gpt-4o"), None, &u);
        assert_eq!(
            (q.cost_micros, q.price_micros, q.price_card_id),
            (1_000_000, 1_200_000, None)
        );
        // model price card +100%
        let q = c.quote(Some("gpt-4o-mini"), None, &u);
        assert_eq!((q.price_micros, q.price_card_id), (2_000_000, Some(99)));
        // team override +50%
        let q = c.quote(Some("gpt-4o-mini"), Some(7), &u);
        assert_eq!((q.price_micros, q.price_card_id), (1_500_000, Some(77)));
    }

    #[test]
    fn unknown_model_is_unpriced() {
        let q = cat().quote(
            Some("mystery"),
            None,
            &Usage {
                input: 5,
                ..Default::default()
            },
        );
        assert!(q.unpriced && q.cost_micros == 0 && q.model_id.is_none());
    }

    #[test]
    fn key_model_allowlist_accepts_canonical_name() {
        let c = cat();
        let allow = vec!["gpt-4o".to_string()];
        assert!(c.allowed(&allow, Some("gpt-4o-2024-08-06")));
        assert!(!c.allowed(&allow, Some("gpt-4o-mini")));
        assert!(!c.allowed(&allow, None));
        assert!(c.allowed(&[], None));
    }
}
