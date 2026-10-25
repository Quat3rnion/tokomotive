//! Token usage in exclusive Token Classes.

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
}

impl Usage {
    /// All input-side tokens. Context Tiers are selected on this value.
    pub fn total_input(&self) -> i64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h + self.image_input
    }

    pub fn total(&self) -> i64 {
        self.total_input() + self.output + self.image_output
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}
