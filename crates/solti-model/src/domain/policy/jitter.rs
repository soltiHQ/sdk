//! Jitter strategy.
//!
//! [`JitterPolicy`] adds randomness to backoff delays to prevent thundering-herd effects.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Controls how random jitter is applied to backoff delays.
///
/// Jitter spreads retries over time.
/// This helps avoid many agents retrying at the same instant.
///
/// | Variant        | Delay range                  | Collision resistance |
/// |----------------|------------------------------|----------------------|
/// | `None`         | exactly `base`               | none (deterministic) |
/// | `Full`         | uniform `[0, base]`          | highest              |
/// | `Equal`        | around half to full base     | moderate             |
/// | `Decorrelated` | `min(max, rand(base * 3))`   | high                 |
///
/// The exact math is implemented in the backoff subsystem; this enum only selects the strategy.
///
/// ## Example
///
/// ```
/// use solti_model::JitterPolicy;
///
/// assert_eq!("equal".parse::<JitterPolicy>().unwrap(), JitterPolicy::Equal);
/// assert_eq!("".parse::<JitterPolicy>().unwrap(), JitterPolicy::Full);
/// ```
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum JitterPolicy {
    /// Full jitter: delay is uniformly sampled from `[0, base]`.
    #[default]
    Full,
    /// No randomness applied. Backoff durations remain fixed.
    None,
    /// Equal jitter: delay is sampled around the midpoint (`base / 2`), providing a balance between stability and randomness.
    Equal,
    /// Decorrelated jitter: delay is sampled from `min(max, rand(base * 3))`.
    Decorrelated,
}

impl FromStr for JitterPolicy {
    type Err = ModelError;
    fn from_str(s: &str) -> ModelResult<Self> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("full") || s.eq_ignore_ascii_case("default") {
            Ok(JitterPolicy::Full)
        } else if s.eq_ignore_ascii_case("none") {
            Ok(JitterPolicy::None)
        } else if s.eq_ignore_ascii_case("equal") {
            Ok(JitterPolicy::Equal)
        } else if s.eq_ignore_ascii_case("decorrelated") {
            Ok(JitterPolicy::Decorrelated)
        } else {
            Err(ModelError::UnknownJitter(s.to_string()))
        }
    }
}
