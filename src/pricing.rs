//! Cost Cards, Price Cards, Context Tiers and the money calculation.
//!
//! All money is integer micro-dollars (1 USD = 1_000_000). Rates are
//! micro-dollars per million tokens, so `$3.00 / Mtok` is `3_000_000`.

use serde::{Deserialize, Serialize};

use crate::usage::Usage;

pub const MICROS_PER_USD: i64 = 1_000_000;

/// Rates per million tokens for each Token Class, in micro-dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rates {
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub cache_read: i64,
    #[serde(default)]
    pub cache_write_5m: i64,
    #[serde(default)]
    pub cache_write_1h: i64,
}

impl Rates {
    /// Apply a markup in basis points (10_000 = +100%).
    pub fn marked_up(&self, markup_bp: i64) -> Rates {
        let f = |r: i64| div_round(r as i128 * (10_000 + markup_bp) as i128, 10_000) as i64;
        Rates {
            input: f(self.input),
            output: f(self.output),
            cache_read: f(self.cache_read),
            cache_write_5m: f(self.cache_write_5m),
            cache_write_1h: f(self.cache_write_1h),
        }
    }

    /// Amount in micro-dollars for the usage. Rounds once, half away from zero.
    pub fn amount(&self, u: &Usage) -> i64 {
        let sum: i128 = u.input as i128 * self.input as i128
            + u.output as i128 * self.output as i128
            + u.cache_read as i128 * self.cache_read as i128
            + u.cache_write_5m as i128 * self.cache_write_5m as i128
            + u.cache_write_1h as i128 * self.cache_write_1h as i128;
        div_round(sum, 1_000_000) as i64
    }
}

/// A band of input size that selects different rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTier {
    /// The tier applies when total input tokens are strictly above this value.
    pub above_input_tokens: i64,
    pub rates: Rates,
}

/// A set of rates with optional Context Tiers. Used for both Cost Cards and
/// explicit Price Cards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRates {
    pub base: Rates,
    #[serde(default)]
    pub tiers: Vec<ContextTier>,
}

impl CardRates {
    /// The rates that apply to this usage: the highest tier whose threshold
    /// the input exceeds, else the base rates.
    pub fn rates_for(&self, u: &Usage) -> Rates {
        let input = u.total_input();
        self.tiers
            .iter()
            .filter(|t| input > t.above_input_tokens)
            .max_by_key(|t| t.above_input_tokens)
            .map(|t| t.rates)
            .unwrap_or(self.base)
    }

    pub fn amount(&self, u: &Usage) -> i64 {
        self.rates_for(u).amount(u)
    }

    pub fn marked_up(&self, markup_bp: i64) -> CardRates {
        CardRates {
            base: self.base.marked_up(markup_bp),
            tiers: self
                .tiers
                .iter()
                .map(|t| ContextTier {
                    above_input_tokens: t.above_input_tokens,
                    rates: t.rates.marked_up(markup_bp),
                })
                .collect(),
        }
    }
}

/// A Price Card is either explicit rates or a Markup over the Cost Card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSpec {
    Rates(CardRates),
    MarkupBp(i64),
}

impl PriceSpec {
    pub fn resolve(&self, cost: &CardRates) -> CardRates {
        match self {
            PriceSpec::Rates(r) => r.clone(),
            PriceSpec::MarkupBp(bp) => cost.marked_up(*bp),
        }
    }
}

fn div_round(n: i128, d: i128) -> i128 {
    if n >= 0 {
        (n + d / 2) / d
    } else {
        (n - d / 2) / d
    }
}

/// Format micro-dollars as a dollar string, e.g. `$1.234567`.
pub fn fmt_usd(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    format!("{sign}${}.{:06}", m / 1_000_000, m % 1_000_000)
}

/// Format a per-Mtok rate as dollars, e.g. `3.00`.
pub fn fmt_rate(micros: i64) -> String {
    let s = format!("{:.6}", micros as f64 / MICROS_PER_USD as f64);
    let s = s.trim_end_matches('0');

    if s.ends_with('.') {
        format!("{s}00")
    } else {
        s.to_string()
    }
}

/// Parse a dollar amount such as `3`, `3.5` or `0.000125` into micro-dollars.
pub fn parse_usd(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches('$');
    if s.is_empty() {
        return Some(0);
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > 6
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac: i64 = format!("{frac:0<6}").parse().ok()?;
    let v = whole.checked_mul(MICROS_PER_USD)?.checked_add(frac)?;
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sonnet() -> CardRates {
        CardRates {
            base: Rates {
                input: 3_000_000,
                output: 15_000_000,
                cache_read: 300_000,
                cache_write_5m: 3_750_000,
                cache_write_1h: 6_000_000,
            },
            tiers: vec![ContextTier {
                above_input_tokens: 200_000,
                rates: Rates {
                    input: 6_000_000,
                    output: 22_500_000,
                    cache_read: 600_000,
                    cache_write_5m: 7_500_000,
                    cache_write_1h: 12_000_000,
                },
            }],
        }
    }

    #[test]
    fn amount_sums_all_token_classes() {
        let u = Usage {
            input: 1000,
            output: 500,
            cache_read: 10_000,
            cache_write_5m: 2000,
            cache_write_1h: 1000,
        };
        // 1000*3 + 500*15 + 10000*0.3 + 2000*3.75 + 1000*6 = 3000+7500+3000+7500+6000
        assert_eq!(sonnet().amount(&u), 27_000);
    }

    #[test]
    fn context_tier_uses_total_input_including_cache() {
        let below = Usage {
            input: 100_000,
            cache_read: 100_000,
            ..Default::default()
        };
        let above = Usage {
            input: 100_001,
            cache_read: 100_000,
            ..Default::default()
        };
        assert_eq!(sonnet().rates_for(&below).input, 3_000_000);
        assert_eq!(sonnet().rates_for(&above).input, 6_000_000);
    }

    #[test]
    fn rounding_is_done_once_per_request() {
        let r = Rates {
            input: 75_000,
            ..Default::default()
        }; // $0.075 / Mtok
        assert_eq!(
            r.amount(&Usage {
                input: 10,
                ..Default::default()
            }),
            1
        ); // 0.75 -> 1
        assert_eq!(
            r.amount(&Usage {
                input: 6,
                ..Default::default()
            }),
            0
        ); // 0.45 -> 0
    }

    #[test]
    fn markup_applies_to_base_and_tiers() {
        let p = PriceSpec::MarkupBp(2_000).resolve(&sonnet());
        assert_eq!(p.base.input, 3_600_000);
        assert_eq!(p.tiers[0].rates.output, 27_000_000);
    }

    #[test]
    fn usd_parse_and_format() {
        assert_eq!(parse_usd("3"), Some(3_000_000));
        assert_eq!(parse_usd("0.000125"), Some(125));
        assert_eq!(parse_usd("$1.5"), Some(1_500_000));
        assert_eq!(parse_usd("1.0000001"), None);
        assert_eq!(fmt_usd(1_234_567), "$1.234567");
        assert_eq!(fmt_usd(-5), "-$0.000005");
        assert_eq!(fmt_rate(3_000_000), "3.00");
        assert_eq!(fmt_rate(125), "0.000125");
    }
}
