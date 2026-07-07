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
///
/// ## Also
///
/// - [`JitterPolicy`](super::JitterPolicy) jitter strategy applied to each delay.
/// - [`RestartPolicy`](super::RestartPolicy) controls *when* to restart; backoff controls *delay*.
/// - [`BackoffPolicy::validate`] parameter validation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "raw::BackoffPolicyRaw")]
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

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct BackoffPolicyRaw {
        pub jitter: super::super::JitterPolicy,
        pub first_ms: u64,
        pub max_ms: u64,
        pub factor: f64,
    }

    impl TryFrom<BackoffPolicyRaw> for BackoffPolicy {
        type Error = ModelError;

        fn try_from(r: BackoffPolicyRaw) -> Result<Self, Self::Error> {
            let p = BackoffPolicy {
                jitter: r.jitter,
                first_ms: r.first_ms,
                max_ms: r.max_ms,
                factor: r.factor,
            };
            p.validate()?;
            Ok(p)
        }
    }
}

impl BackoffPolicy {
    /// Validate backoff parameters.
    ///
    /// Checks:
    /// - `first_ms > 0`
    /// - `max_ms >= first_ms`
    /// - `factor >= 1.0` and finite
    ///
    /// ## Errors
    ///
    /// - [`ModelError::Invalid`]: `first_ms` is zero, `max_ms` is less than `first_ms`,
    ///   or `factor` is not finite or below `1.0`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_defaults() {
        assert!(BackoffPolicy::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_first_ms() {
        let p = BackoffPolicy {
            first_ms: 0,
            ..BackoffPolicy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_max_smaller_than_first() {
        let p = BackoffPolicy {
            first_ms: 500,
            max_ms: 100,
            ..BackoffPolicy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_factor_below_one() {
        let p = BackoffPolicy {
            factor: 0.5,
            ..BackoffPolicy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_nan_factor() {
        let p = BackoffPolicy {
            factor: f64::NAN,
            ..BackoffPolicy::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn serde_roundtrip_accepts_valid() {
        let p = BackoffPolicy::default();
        let json = serde_json::to_string(&p).unwrap();
        let back: BackoffPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn serde_rejects_invalid_first_ms_on_deserialize() {
        let json = r#"{"jitter":"full","firstMs":0,"maxMs":30000,"factor":2.0}"#;
        let err = serde_json::from_str::<BackoffPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("first_ms"), "got: {err}");
    }

    #[test]
    fn serde_rejects_inverted_max_on_deserialize() {
        let json = r#"{"jitter":"full","firstMs":1000,"maxMs":500,"factor":2.0}"#;
        let err = serde_json::from_str::<BackoffPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("max_ms"), "got: {err}");
    }

    #[test]
    fn serde_rejects_factor_below_one_on_deserialize() {
        let json = r#"{"jitter":"full","firstMs":1000,"maxMs":30000,"factor":0.5}"#;
        let err = serde_json::from_str::<BackoffPolicy>(json).unwrap_err();
        assert!(err.to_string().contains("factor"), "got: {err}");
    }
}
