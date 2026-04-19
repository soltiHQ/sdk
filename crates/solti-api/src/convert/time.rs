//! # Time encoding utilities.

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Convert a `SystemTime` to Unix ms.
///
/// Negative instants (before `UNIX_EPOCH`) are logged and fall back to `0`.
pub(super) fn system_time_to_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_else(|e| {
            warn!(error = %e, "system time is before unix epoch, defaulting to 0");
            Duration::ZERO
        })
        .as_millis() as i64
}
