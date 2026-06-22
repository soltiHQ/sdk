//! Metrics interface for the discovery heartbeat task.
//!
//! Implement [`DiscoverMetricsBackend`] to record discovery lifecycle events.

use std::sync::Arc;

/// Canonical `outcome` label values (success / failure categorization).
pub const OUTCOME_SUCCESS: &str = "success";
/// See [`OUTCOME_SUCCESS`].
pub const OUTCOME_FAILURE: &str = "failure";

/// Canonical `reason` label values for heartbeat failures.
///
/// Use these constants as `&'static str` inputs to
/// [`DiscoverMetricsBackend::record_failure`] so label cardinality stays bounded.
pub const FAIL_CONNECT: &str = "connect";
/// Transport / network timeout.
pub const FAIL_TIMEOUT: &str = "timeout";
/// Server returned a 4xx-equivalent (client-side problem: bad request, not-found, already-exists, ...).
pub const FAIL_REJECTED_CLIENT: &str = "rejected_client";
/// Server returned a 5xx-equivalent (server-side problem: internal, unavailable, ...).
pub const FAIL_REJECTED_SERVER: &str = "rejected_server";
/// Response body couldn't be decoded.
pub const FAIL_PARSE: &str = "parse";
/// Authentication rejected.
pub const FAIL_AUTH: &str = "auth";
/// Anything else.
pub const FAIL_OTHER: &str = "other";

/// Metrics backend for the discovery heartbeat task.
///
/// All methods have empty default bodies so integrators can override only the hooks they care about.
pub trait DiscoverMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Called once per sync attempt before the network call starts.
    fn record_attempt(&self) {}

    /// Called when a sync attempt succeeds.
    fn record_success(&self, _duration_ms: u64) {}

    /// Called when a sync attempt fails, with the canonical reason label.
    fn record_failure(&self, _duration_ms: u64, _reason: &'static str) {}

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
