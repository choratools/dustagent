//! Bounded provider-only retry policy. Tool calls are never retried by this policy.
use crate::{DustError, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub base_delay_ms: u64,
}
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay_ms: 1_000,
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
    /// Uses the larger of local exponential backoff and the provider's minimum delay.
    pub fn delay_with_retry_after(&self, index: usize, retry_after: Option<Duration>) -> Duration {
        self.delay(index).max(retry_after.unwrap_or_default())
    }
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Parses Retry-After as delta-seconds or an HTTP date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_at
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_supports_seconds_and_http_dates() {
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("nonsense"), None);
        assert_eq!(parse_retry_after("-1"), None);

        let future = SystemTime::now() + Duration::from_secs(30);
        let parsed = parse_retry_after(&httpdate::fmt_http_date(future)).unwrap();
        assert!((28..=30).contains(&parsed.as_secs()));
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(5));
        assert_eq!(parse_retry_after(&past), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_is_a_minimum_over_exponential_backoff() {
        let config = RetryConfig::default();
        assert_eq!(config.delay(0), Duration::from_secs(1));
        assert_eq!(config.delay(1), Duration::from_secs(2));
        assert_eq!(
            config.delay_with_retry_after(0, Some(Duration::from_secs(8))),
            Duration::from_secs(8)
        );
        assert_eq!(
            config.delay_with_retry_after(2, Some(Duration::from_secs(1))),
            Duration::from_secs(4)
        );
    }
}
