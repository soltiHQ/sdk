//! # Agent uptime
//!
//! [`UptimeSource`] supplies the value sent in each discovery request.
//! The application chooses the uptime epoch.
//!
//! ```text
//! application lifecycle boundary
//!             │ creates
//!             ▼
//!       UptimeSource
//!             │ read on every attempt
//!             ▼
//! SyncRequest.uptime_seconds
//! ```
//!
//! [`MonotonicUptime`] starts its epoch at construction.
//! Wall-clock changes do not affect it.

use std::time::Instant;

/// Supplies the monotonic elapsed time reported by discovery heartbeats.
///
/// The composition root owns the epoch.
/// It passes a shared source to [`sync`](crate::sync).
///
/// A closure returning `u64` also implements this trait.
pub trait UptimeSource: Send + Sync + 'static {
    /// Returns elapsed whole seconds since the application-owned epoch.
    ///
    /// Discovery rejects values above `i64::MAX` before transport because the
    /// discovery v1 wire field is a signed 64-bit integer.
    fn uptime_seconds(&self) -> u64;
}

impl<F> UptimeSource for F
where
    F: Fn() -> u64 + Send + Sync + 'static,
{
    fn uptime_seconds(&self) -> u64 {
        self()
    }
}

/// Monotonic uptime clock starting at construction.
///
/// Create it at the chosen agent lifecycle boundary.
/// Share it with discovery through [`std::sync::Arc`].
///
/// It is independent of `solti-core` construction.
#[derive(Debug)]
pub struct MonotonicUptime {
    started_at: Instant,
}

impl MonotonicUptime {
    /// Creates a monotonic uptime clock with a new epoch.
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl Default for MonotonicUptime {
    fn default() -> Self {
        Self::new()
    }
}

impl UptimeSource for MonotonicUptime {
    fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn monotonic_uptime_uses_its_own_epoch() {
        let source = MonotonicUptime {
            started_at: Instant::now() - Duration::from_secs(5),
        };

        assert!(source.uptime_seconds() >= 5);
    }

    #[test]
    fn closure_can_supply_deterministic_uptime() {
        let source = || 42;

        assert_eq!(source.uptime_seconds(), 42);
    }
}
