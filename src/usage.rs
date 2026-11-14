//! Token usage in exclusive Token Classes, plus non-token Service units.

use serde::{Deserialize, Serialize};

/// Token counts for one request. Each class is exclusive: `input` holds only
/// uncached input tokens, and no token is counted in two classes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    #[serde(default)]
    pub image_input: i64,
    #[serde(default)]
    pub image_output: i64,
    /// Non-token units, defined by the endpoint (for example search units).
    #[serde(default)]
    pub service_units: i64,
}

impl Usage {
    /// The largest token count that one class of one request can hold.
    /// Upstream values above it are malformed; this bound keeps every sum
    /// and price product far from i64 overflow.
    pub const MAX_CLASS: i64 = 1_000_000_000_000;

    /// Clamp each class to `0..=MAX_CLASS`. Apply it to every usage that an
    /// upstream reports, before accounting.
    pub fn bounded(self) -> Usage {
        let c = |v: i64| v.clamp(0, Self::MAX_CLASS);
        Usage {
            input: c(self.input),
            output: c(self.output),
            cache_read: c(self.cache_read),
            cache_write_5m: c(self.cache_write_5m),
            cache_write_1h: c(self.cache_write_1h),
            image_input: c(self.image_input),
            image_output: c(self.image_output),
            service_units: c(self.service_units),
        }
    }

    /// All input-side tokens. Context Tiers are selected on this value.
    pub fn total_input(&self) -> i64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write_5m)
            .saturating_add(self.cache_write_1h)
            .saturating_add(self.image_input)
    }

    pub fn total(&self) -> i64 {
        self.total_input()
            .saturating_add(self.output)
            .saturating_add(self.image_output)
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.service_units == 0
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::Usage;

    #[test]
    fn huge_counts_neither_panic_nor_wrap() {
        let u = Usage {
            input: i64::MAX,
            output: i64::MAX,
            cache_read: i64::MAX,
            cache_write_5m: i64::MAX,
            cache_write_1h: i64::MAX,
            image_input: i64::MAX,
            image_output: i64::MAX,
            service_units: i64::MAX,
        }
        .bounded();
        assert!(u.total() > 0);
        assert!(u.total_input() > 0);
        assert!(u.input <= Usage::MAX_CLASS);
        // Unbounded input plus cache used to wrap negative and select the
        // base Context Tier instead of the highest one.
        let raw = Usage {
            input: i64::MAX,
            cache_read: i64::MAX,
            ..Default::default()
        };
        assert_eq!(raw.total_input(), i64::MAX, "saturates, never wraps");
        let neg = Usage {
            input: -5,
            output: -1,
            ..Default::default()
        }
        .bounded();
        assert_eq!((neg.input, neg.output), (0, 0));
    }
}
