use serde::{Deserialize, Serialize};

/// Defines how backoff delays are calculated when retrying or restarting a task.
///
/// This structure combines:
/// - exponential backoff parameters (`first_ms`, `max_ms`, `factor`)
/// - optional fixed delay (`delay_ms`)
/// - a jitter policy (`jitter`)
///
/// ## Fields
/// - `jitter` — Jitter strategy applied to every computed delay.
///   Helps avoid synchronized retry storms.
/// - `first_ms` — Initial backoff delay (in milliseconds)
///   used for the first retry attempt when `delay_ms` is not set.
/// - `max_ms` — Maximum allowed delay (in milliseconds).
///   The exponential backoff will never exceed this cap.
/// - `factor` — Multiplier for exponential growth.
///   For example:
///   - `factor = 2.0` → classic doubling (100 → 200 → 400 → ...)
///   - `factor = 1.0` → linear growth
///   - `factor < 1.0` → decaying backoff (rare, but allowed)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackoffStrategy {
    /// Jitter policy applied to each computed delay.
    pub jitter: super::JitterStrategy,
    /// Initial delay (ms) for exponential backoff.
    pub first_ms: u64,
    /// Maximum allowed delay (ms).
    pub max_ms: u64,
    /// Exponential growth multiplier.
    pub factor: f64,
}
