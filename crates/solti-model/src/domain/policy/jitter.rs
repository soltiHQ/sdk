//! # Jitter policy
//!
//! [`JitterPolicy`] selects how retry delay randomness is applied.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Jitter applied to a backoff delay.
///
/// | Variant        | Delay range                              |
/// |----------------|------------------------------------------|
/// | `None`         | `base`                                   |
/// | `Full`         | `[0, base]`                              |
/// | `Equal`        | `[base / 2, base]`                       |
/// | `Decorrelated` | `[first, min(base * 3, max)]`            |
///
/// The execution layer applies the selected policy.
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
    /// Equal jitter samples from `[base / 2, base]`.
    Equal,
    /// Memoryless randomized band: delay is sampled uniformly from
    /// `[first, min(base * 3, max)]`.
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
