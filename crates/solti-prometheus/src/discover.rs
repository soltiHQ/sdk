//! # Discovery metrics
//!
//! [`PrometheusDiscoverMetrics`] implements [`DiscoverMetricsBackend`].
//! It records control-plane heartbeat attempts and retry holds.
//!
//! Enable it with the `discover` feature.
//!
//! ## Flow
//!
//! ```text
//! Discovery task ──► DiscoverMetricsBackend ──► PrometheusDiscoverMetrics ──► Registry
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{Counter, CounterVec, Gauge, Histogram, HistogramVec, Registry};
use solti_discover::{
    DiscoverFailReason, DiscoverMetricsBackend, OUTCOME_FAILURE, OUTCOME_SUCCESS,
};

use crate::register::{MetricGroup, ms_to_secs};

/// Prometheus discovery metrics.
///
/// ## Metrics
///
/// | Metric                                          | Type      | Labels    | Description                         |
/// |-------------------------------------------------|-----------|-----------|-------------------------------------|
/// | `solti_discover_attempts_total`                 | Counter   | -         | Total sync attempts                 |
/// | `solti_discover_outcomes_total`                 | Counter   | `outcome` | Outcomes (`success` / `failure`)    |
/// | `solti_discover_duration_seconds`               | Histogram | `outcome` | Sync call duration                  |
/// | `solti_discover_failures_total`                 | Counter   | `reason`  | Failures grouped by reason          |
/// | `solti_discover_last_success_timestamp_seconds` | Gauge     | -         | UNIX time of last successful sync   |
/// | `solti_discover_holds_total`                    | Counter   | -         | Server-advised retry holds received |
/// | `solti_discover_hold_duration_seconds`          | Histogram | -         | Duration of advised holds           |
///
/// ## Event Mapping
///
/// ```text
/// record_attempt()
///   └──► attempts_total
///
/// record_success(duration_ms)
///   ├──► outcomes_total{outcome="success"}
///   ├──► duration_seconds{outcome="success"}
///   └──► last_success_timestamp_seconds
///
/// record_failure(duration_ms, reason)
///   ├──► outcomes_total{outcome="failure"}
///   ├──► duration_seconds{outcome="failure"}
///   └──► failures_total{reason}
///
/// record_hold(duration_s)
///   ├──► holds_total
///   └──► hold_duration_seconds
/// ```
///
/// ## Rules
///
/// - Failure labels come from [`DiscoverFailReason::as_label`].
/// - Attempt durations enter the backend in milliseconds.
/// - Hold duration already enters the backend in seconds.
/// - A success records the current UNIX timestamp.
/// - A clock value before the UNIX epoch records `0`.
/// - Duration histograms export seconds.
///
/// ## Example
///
/// ```
/// use solti_discover::{DiscoverFailReason, DiscoverMetricsBackend};
/// use solti_prometheus::{PrometheusDiscoverMetrics, Registry};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
/// let registry = Registry::new();
/// let metrics = PrometheusDiscoverMetrics::new(&registry)?;
///
/// metrics.record_attempt();
/// metrics.record_success(25);
/// metrics.record_failure(50, DiscoverFailReason::Timeout);
/// metrics.record_hold(10);
///
/// assert!(!registry.gather().is_empty());
/// # Ok(()) }
/// ```
pub struct PrometheusDiscoverMetrics {
    attempts_total: Counter,
    outcomes_total: CounterVec,
    duration_seconds: HistogramVec,
    failures_total: CounterVec,
    last_success_ts: Gauge,
    holds_total: Counter,
    hold_duration_seconds: Histogram,
}

impl PrometheusDiscoverMetrics {
    /// Creates a discovery metrics backend and registers its collectors.
    ///
    /// The returned backend updates the collectors in `registry`.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the metric group cannot be created or registered.
    /// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let mut metrics = MetricGroup::new();

        let attempts_total = metrics.counter(
            "discover",
            "attempts_total",
            "Total discovery heartbeat attempts",
        )?;
        let outcomes_total = metrics.counter_vec(
            "discover",
            "outcomes_total",
            "Discovery heartbeat outcomes",
            &["outcome"],
        )?;
        let duration_seconds = metrics.histogram_vec(
            "discover",
            "duration_seconds",
            "Discovery heartbeat call duration",
            vec![0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0],
            &["outcome"],
        )?;
        let failures_total = metrics.counter_vec(
            "discover",
            "failures_total",
            "Discovery heartbeat failures grouped by reason",
            &["reason"],
        )?;
        let last_success_ts = metrics.gauge(
            "discover",
            "last_success_timestamp_seconds",
            "UNIX timestamp of the last successful heartbeat",
        )?;
        let holds_total = metrics.counter(
            "discover",
            "holds_total",
            "Server-advised retry holds observed",
        )?;
        let hold_duration_seconds = metrics.histogram(
            "discover",
            "hold_duration_seconds",
            "Duration of server-advised retry holds",
            vec![1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0],
        )?;
        metrics.register(registry)?;

        Ok(Self {
            attempts_total,
            outcomes_total,
            duration_seconds,
            failures_total,
            last_success_ts,
            holds_total,
            hold_duration_seconds,
        })
    }
}

impl std::fmt::Debug for PrometheusDiscoverMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusDiscoverMetrics").finish()
    }
}

impl DiscoverMetricsBackend for PrometheusDiscoverMetrics {
    fn record_attempt(&self) {
        self.attempts_total.inc();
    }

    fn record_success(&self, duration_ms: u64) {
        self.outcomes_total
            .with_label_values(&[OUTCOME_SUCCESS])
            .inc();
        self.duration_seconds
            .with_label_values(&[OUTCOME_SUCCESS])
            .observe(ms_to_secs(duration_ms));
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.last_success_ts.set(ts);
    }

    fn record_failure(&self, duration_ms: u64, reason: DiscoverFailReason) {
        self.outcomes_total
            .with_label_values(&[OUTCOME_FAILURE])
            .inc();
        self.duration_seconds
            .with_label_values(&[OUTCOME_FAILURE])
            .observe(ms_to_secs(duration_ms));
        self.failures_total
            .with_label_values(&[reason.as_label()])
            .inc();
    }

    fn record_hold(&self, duration_s: u64) {
        self.holds_total.inc();
        self.hold_duration_seconds.observe(duration_s as f64);
    }
}
