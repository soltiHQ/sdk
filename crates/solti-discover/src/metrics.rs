//! Metrics interface for the discovery heartbeat task.
//!
//! Implement [`DiscoverMetricsBackend`] to record discovery lifecycle events.

use std::sync::Arc;

/// Canonical `outcome` label values (success / failure categorization).
pub const OUTCOME_SUCCESS: &str = "success";
/// See [`OUTCOME_SUCCESS`].
pub const OUTCOME_FAILURE: &str = "failure";

/// Canonical `reason` label for heartbeat failures.
///
/// A bounded, typed set (mirroring the `as_label()` enums in the other crates) so the
/// failure-metric label cardinality stays fixed regardless of the underlying error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscoverFailReason {
    /// Could not establish a connection.
    Connect,
    /// Transport / network timeout.
    Timeout,
    /// Server returned a 4xx-equivalent (client-side: bad request, not-found, already-exists, …).
    RejectedClient,
    /// Server returned a 5xx-equivalent (server-side: internal, unavailable, …).
    RejectedServer,
    /// Response body couldn't be decoded.
    Parse,
    /// Authentication rejected.
    Auth,
    /// Anything else.
    Other,
}

impl DiscoverFailReason {
    /// Stable, low-cardinality metric label for this reason.
    pub fn as_label(self) -> &'static str {
        match self {
            DiscoverFailReason::Connect => "connect",
            DiscoverFailReason::Timeout => "timeout",
            DiscoverFailReason::RejectedClient => "rejected_client",
            DiscoverFailReason::RejectedServer => "rejected_server",
            DiscoverFailReason::Parse => "parse",
            DiscoverFailReason::Auth => "auth",
            DiscoverFailReason::Other => "other",
        }
    }
}

/// Metrics backend for the discovery heartbeat task.
///
/// All methods have empty default bodies so integrators can override only the hooks they care about.
pub trait DiscoverMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Called once per sync attempt before the network call starts.
    fn record_attempt(&self) {}

    /// Called when a sync attempt succeeds.
    fn record_success(&self, _duration_ms: u64) {}

    /// Called when a sync attempt fails, with the canonical typed reason.
    fn record_failure(&self, _duration_ms: u64, _reason: DiscoverFailReason) {}

    /// Called when the server advised a retry hold (`retry_after_s`), with the clamped value in seconds.
    fn record_hold(&self, _duration_s: u64) {}
}

/// Zero-cost default implementation that discards all events.
#[derive(Debug, Default)]
pub struct NoOpDiscoverMetrics;

impl DiscoverMetricsBackend for NoOpDiscoverMetrics {}

/// Shareable handle used throughout this crate.
pub type DiscoverMetricsHandle = Arc<dyn DiscoverMetricsBackend>;

/// Construct a no-op handle - convenient default for `DiscoverConfig` (feature `grpc`/`http`).
pub fn noop_discover_metrics() -> DiscoverMetricsHandle {
    Arc::new(NoOpDiscoverMetrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_fail_reason_as_label_maps_all_variants() {
        assert_eq!(DiscoverFailReason::Connect.as_label(), "connect");
        assert_eq!(DiscoverFailReason::Timeout.as_label(), "timeout");
        assert_eq!(
            DiscoverFailReason::RejectedClient.as_label(),
            "rejected_client"
        );
        assert_eq!(
            DiscoverFailReason::RejectedServer.as_label(),
            "rejected_server"
        );
        assert_eq!(DiscoverFailReason::Parse.as_label(), "parse");
        assert_eq!(DiscoverFailReason::Auth.as_label(), "auth");
        assert_eq!(DiscoverFailReason::Other.as_label(), "other");
    }
}
