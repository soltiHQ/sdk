//! Discovery uptime sources.

use std::time::Instant;

/// Supplies the monotonic elapsed time reported by discovery heartbeats.
///
/// The composition root owns the epoch and passes the source to
/// [`sync`](crate::sync). Custom implementations make heartbeat stamping
/// deterministic in tests and allow a host to align the value with its own
/// lifecycle definition.
pub trait UptimeSource: Send + Sync + 'static {
    /// Return elapsed whole seconds since the composition-owned epoch.
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
/// Create it at the host's chosen agent-composition boundary, then share it
/// with discovery through an [`std::sync::Arc`]. It is independent of wall
/// clock adjustments and of `solti-core` supervisor construction.
#[derive(Debug)]
pub struct MonotonicUptime {
    started_at: Instant,
}

impl MonotonicUptime {
    /// Start a new monotonic uptime clock.
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
