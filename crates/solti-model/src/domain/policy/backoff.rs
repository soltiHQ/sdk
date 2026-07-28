//! # Backoff policy
//!
//! [`BackoffPolicy`] configures retry delay growth and jitter.
//! Direct deserialization validates fields.
//! Struct literals must be checked with [`BackoffPolicy::validate`].

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
/// Before jitter, factor `2.0` grows `1s, 2s, 4s, 8s`.
/// Growth stops at `max_ms`.
///
/// ## Example
///
/// ```
/// use solti_model::{BackoffPolicy, JitterPolicy};
///
/// let backoff = BackoffPolicy {
///     jitter: JitterPolicy::Equal,
///     first_ms: 1_000,
///     max_ms: 30_000,
///     factor: 2.0,
/// };
///
/// backoff.validate().unwrap();
/// ```
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
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    /// Validates backoff parameters.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `first_ms` is zero,
    /// `max_ms` is below `first_ms`, or `factor` is not finite or below `1.0`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::BackoffPolicy;
    ///
    /// let mut backoff = BackoffPolicy::default();
    /// backoff.first_ms = 0;
    ///
    /// assert!(backoff.validate().is_err());
    /// ```
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
    /// Returns full jitter, a 1-second initial delay, a 30-second cap, and factor `2.0`.
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
    fn validation_accepts_defaults_and_rejects_invalid_fields() {
        BackoffPolicy::default().validate().unwrap();

        for invalid in [
            BackoffPolicy {
                first_ms: 0,
                ..BackoffPolicy::default()
            },
            BackoffPolicy {
                first_ms: 500,
                max_ms: 100,
                ..BackoffPolicy::default()
            },
            BackoffPolicy {
                factor: 0.5,
                ..BackoffPolicy::default()
            },
            BackoffPolicy {
                factor: f64::NAN,
                ..BackoffPolicy::default()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn serde_roundtrip_accepts_valid_policy() {
        let policy = BackoffPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let back: BackoffPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, policy);
    }

    #[test]
    fn serde_rejects_every_invalid_field() {
        for (json, field) in [
            (
                r#"{"jitter":"full","firstMs":0,"maxMs":30000,"factor":2.0}"#,
                "first_ms",
            ),
            (
                r#"{"jitter":"full","firstMs":1000,"maxMs":500,"factor":2.0}"#,
                "max_ms",
            ),
            (
                r#"{"jitter":"full","firstMs":1000,"maxMs":30000,"factor":0.5}"#,
                "factor",
            ),
        ] {
            let error = serde_json::from_str::<BackoffPolicy>(json).unwrap_err();
            assert!(error.to_string().contains(field), "got: {error}");
        }
    }
}
