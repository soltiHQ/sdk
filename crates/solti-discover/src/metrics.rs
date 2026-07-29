//! # Discovery metrics
//!
//! [`DiscoverMetricsBackend`] receives discovery lifecycle measurements.
//! The default backend discards them.
//!
//! ```text
//! sync attempt
//!      ├──► record_attempt
//!      ├──► record_success(duration)
//!      ├──► record_failure(duration, reason)
//!      └──► record_hold(seconds)
//! ```
//!
//! [`DiscoverFailReason`] keeps failure label cardinality bounded.
//! Transport error text is never used as a metric label.

use std::sync::Arc;

/// Canonical `outcome` label value for a successful sync attempt.
pub const OUTCOME_SUCCESS: &str = "success";
/// Canonical `outcome` label value for a failed sync attempt.
pub const OUTCOME_FAILURE: &str = "failure";

/// Canonical `reason` label for heartbeat failures.
///
/// The set remains bounded regardless of transport error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscoverFailReason {
    /// Connection could not be established.
    Connect,
    /// Transport operation timed out.
    Timeout,
    /// Control plane returned a client-side failure.
    RejectedClient,
    /// Control plane returned a server-side failure.
    RejectedServer,
    /// Response body could not be decoded.
    Parse,
    /// Authentication was rejected.
    Auth,
    /// Failure has no more specific category.
    Other,
}

impl DiscoverFailReason {
    /// Returns the stable metric label.
    pub fn as_label(self) -> &'static str {
        match self {
            DiscoverFailReason::RejectedClient => "rejected_client",
            DiscoverFailReason::RejectedServer => "rejected_server",
            DiscoverFailReason::Connect => "connect",
            DiscoverFailReason::Timeout => "timeout",
            DiscoverFailReason::Parse => "parse",
            DiscoverFailReason::Other => "other",
            DiscoverFailReason::Auth => "auth",
        }
    }
}

/// Metrics backend for the discovery heartbeat task.
///
/// Every method has an empty default body.
/// Implementations can override only the required hooks.
pub trait DiscoverMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Records one transport attempt.
    fn record_attempt(&self) {}

    /// Records a successful transport attempt in milliseconds.
    fn record_success(&self, _duration_ms: u64) {}

    /// Records a failed transport attempt in milliseconds.
    fn record_failure(&self, _duration_ms: u64, _reason: DiscoverFailReason) {}

    /// Records a clamped server-advised hold in seconds.
    fn record_hold(&self, _duration_s: u64) {}
}

/// No-op metrics backend.
#[derive(Debug, Default)]
pub struct NoOpDiscoverMetrics;

impl DiscoverMetricsBackend for NoOpDiscoverMetrics {}

/// Shared discovery metrics backend.
pub type DiscoverMetricsHandle = Arc<dyn DiscoverMetricsBackend>;

/// Creates a no-op metrics handle.
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
