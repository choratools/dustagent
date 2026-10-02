//! Bounded provider-only retry policy. Tool calls are never retried by this policy.
use crate::{DustError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub base_delay_ms: u64,
}
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 250,
        }
    }
}
impl RetryConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_retries > 5 || !(1..=10_000).contains(&self.base_delay_ms) {
            return Err(DustError::Config(
                "Provider retries require max_retries <= 5 and base_delay_ms in 1..=10000".into(),
            ));
        }
        Ok(())
    }
    /// Zero-based retry index; bounded even for invalid external indices.
    pub fn delay(&self, index: usize) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.base_delay_ms
                .saturating_mul(1u64 << index.min(5))
                .min(10_000),
        )
    }
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
