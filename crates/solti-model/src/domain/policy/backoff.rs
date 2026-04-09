//! # Backoff policy.
//!
//! [`BackoffPolicy`] controls retry delay growth: initial delay, max cap, factor, and jitter.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};

/// Exponential backoff configuration for task restart delays.
///
/// | Field      | Type           | Default   | Description                           |
/// |------------|----------------|-----------|---------------------------------------|
/// | `jitter`   | `JitterPolicy` | `Full`    | Randomness applied to each delay      |
/// | `first_ms` | `u64`          | `1_000`   | Initial delay (ms)                    |
/// | `max_ms`   | `u64`          | `30_000`  | Maximum delay cap (ms)                |
/// | `factor`   | `f64`          | `2.0`     | Exponential growth multiplier         |
///
/// Growth example with `factor = 2.0`: 1 s → 2 s → 4 s → 8 s → … → 30 s (capped).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackoffPolicy {
    /// Jitter policy applied to each computed delay.
    pub jitter: super::JitterPolicy,
    /// Initial delay (ms) for exponential backoff.
    pub first_ms: u64,
    /// Maximum allowed delay (ms).
    pub max_ms: u64,
    /// Exponential growth multiplier.
    pub factor: f64,
}

impl BackoffPolicy {
    /// Validate backoff parameters.
    ///
    /// Checks:
    /// - `first_ms > 0`
    /// - `max_ms >= first_ms`
    /// - `factor >= 1.0` and finite
    pub fn validate(&self) -> ModelResult<()> {
        if self.first_ms == 0 {
            return Err(ModelError::Invalid(Cow::Borrowed(
                "backoff first_ms must be greater than zero",
            )));
        }
        if self.max_ms < self.first_ms {
            return Err(ModelError::Invalid(Cow::Borrowed(
                "backoff max_ms must be >= first_ms",
            )));
        }
        if !self.factor.is_finite() || self.factor < 1.0 {
            return Err(ModelError::Invalid(Cow::Borrowed(
                "backoff factor must be finite and >= 1.0",
            )));
        }
        Ok(())
    }
}

impl PartialEq for BackoffPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.jitter == other.jitter
            && self.factor.to_bits() == other.factor.to_bits()
            && self.first_ms == other.first_ms
            && self.max_ms == other.max_ms
    }
}

impl Eq for BackoffPolicy {}

impl Hash for BackoffPolicy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.factor.to_bits().hash(state);
        self.first_ms.hash(state);
        self.jitter.hash(state);
        self.max_ms.hash(state);
    }
}

impl Default for BackoffPolicy {
    /// Returns a sensible default: full jitter, 1s initial, 30s max, factor 2.
    fn default() -> Self {
        Self {
            jitter: super::JitterPolicy::Full,
            first_ms: 1_000,
            max_ms: 30_000,
            factor: 2.0,
        }
    }
}
