use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Controls how random jitter is applied to backoff delays.
///
/// Jitter is used to distribute retries over time, preventing synchronized “retry storms” when many tasks fail simultaneously.
/// Different strategies provide different trade-offs between predictability and collision avoidance.
///
/// Strategies:
/// - `None`: No jitter. Backoff durations are deterministic.
/// - `Full`: Full jitter, picks a random delay in `[0, base]`.
/// - `Equal`: Equal jitter, picks a delay around `base/2 ± (base/2 * rand)`.
/// - `Decorrelated`: Decorrelated jitter (a.k.a. "decorrelated exponential"), commonly used to avoid coordinated retries while still converging.
///
/// The exact math is implemented in the backoff subsystem. This enum only specifies the policy.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JitterStrategy {
    /// No randomness applied. Backoff durations remain fixed.
    None,
    /// Full jitter: delay is uniformly sampled from `[0, base]`.
    ///
    /// This is the most collision-resistant strategy.
    #[default]
    Full,
    /// Equal jitter: delay is sampled around the midpoint (`base / 2`), providing a balance between stability and randomness.
    Equal,
    /// Decorrelated jitter: delay is sampled from `min(max, rand(base * 3))`.
    Decorrelated,
}

impl FromStr for JitterStrategy {
    type Err = ModelError;
    fn from_str(s: &str) -> ModelResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "equal" => Ok(JitterStrategy::Equal),
            "" | "none" => Ok(JitterStrategy::None),
            "full" | "default" => Ok(JitterStrategy::Full),
            "decorrelated" => Ok(JitterStrategy::Decorrelated),
            other => Err(ModelError::UnknownJitter(other.to_string())),
        }
    }
}
