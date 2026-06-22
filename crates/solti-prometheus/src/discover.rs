//! # Discovery-heartbeat Prometheus metrics (feature `discover`).
//!
//! [`PrometheusDiscoverMetrics`] implements [`solti_discover::DiscoverMetricsBackend`], exposing `solti_discover_*`
//! attempt / outcome / duration / failure / hold metrics for the control-plane discovery heartbeat.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{Counter, CounterVec, Gauge, Histogram, HistogramVec, Registry};
use solti_discover::{
    DiscoverFailReason, DiscoverMetricsBackend, OUTCOME_FAILURE, OUTCOME_SUCCESS,
};

use crate::register::{Sub, ms_to_secs};

/// Prometheus implementation of [`DiscoverMetricsBackend`].
///
/// ## Metrics (`solti_discover_*`)
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
    /// Register all discovery metrics into `registry`.
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let r = Sub::new(&registry, "discover");

        let attempts_total = r.counter("attempts_total", "Total discovery heartbeat attempts")?;
        let outcomes_total = r.counter_vec(
            "outcomes_total",
            "Discovery heartbeat outcomes",
            &["outcome"],
        )?;
        let duration_seconds = r.histogram_vec(
            "duration_seconds",
            "Discovery heartbeat call duration",
            vec![0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0],
            &["outcome"],
        )?;
        let failures_total = r.counter_vec(
            "failures_total",
            "Discovery heartbeat failures grouped by reason",
            &["reason"],
        )?;
        let last_success_ts = r.gauge(
            "last_success_timestamp_seconds",
            "UNIX timestamp of the last successful heartbeat",
        )?;
        let holds_total = r.counter("holds_total", "Server-advised retry holds observed")?;
        let hold_duration_seconds = r.histogram(
            "hold_duration_seconds",
            "Duration of server-advised retry holds",
            vec![1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0],
        )?;

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
