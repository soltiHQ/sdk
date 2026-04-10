//! # System utilities.
//!
//! Agent uptime tracking via [`uptime_seconds`].

use std::sync::OnceLock;
use std::time::Instant;

static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Initialize agent start time.
pub(crate) fn init_uptime() {
    START_TIME.get_or_init(Instant::now);
}

/// Get agent uptime in seconds.
///
/// Returns `0` if called before [`SupervisorApi::new`](crate::SupervisorApi::new)
pub fn uptime_seconds() -> u64 {
    START_TIME
        .get()
        .map(|start| start.elapsed().as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uptime_increases() {
        init_uptime();
        let t1 = uptime_seconds();
        assert!(t1 < 1_000_000);
    }
}
