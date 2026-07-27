//! # API metrics
//!
//! [`PrometheusApiMetrics`] implements [`ApiMetricsBackend`].
//! It records HTTP and gRPC request traffic.
//!
//! Enable it with the `api` feature.
//!
//! ## Flow
//!
//! ```text
//! HTTP middleware ─┐
//!                  ├──► ApiMetricsBackend ──► PrometheusApiMetrics ──► Registry
//! gRPC service ────┘
//! ```

use prometheus::{CounterVec, GaugeVec, HistogramVec, Registry};
use solti_api::{ApiMetricsBackend, Transport};

use crate::register::{MetricGroup, ms_to_secs};

/// Prometheus API metrics.
///
/// ## Metrics
///
/// | Metric                               | Type      | Labels                                  |
/// |--------------------------------------|-----------|-----------------------------------------|
/// | `solti_api_requests_total`           | Counter   | `transport`, `method`, `path`, `status` |
/// | `solti_api_request_duration_seconds` | Histogram | `transport`, `method`, `path`           |
/// | `solti_api_in_flight_requests`       | Gauge     | `transport`                             |
///
/// ## Labels
///
/// `transport` is `http` or `grpc`.
/// HTTP `path` values are matched route templates.
/// gRPC `path` values are full service method paths.
///
/// ```text
/// HTTP: /apis/solti.io/v1/tasks/{name}
/// gRPC: /solti.task.v1.TaskService/CreateTask
/// ```
///
/// `solti-api` supplies route templates and service method paths.
/// This backend stores the provided labels without normalizing them.
///
/// ## Rules
///
/// - Construction registers all three collectors as one group.
/// - Request durations enter the backend in milliseconds.
/// - Histograms export those durations in seconds.
/// - In-flight changes are applied as signed deltas.
///
/// ## Example
///
/// ```
/// use solti_api::{ApiMetricsBackend, Transport};
/// use solti_prometheus::{PrometheusApiMetrics, Registry};
///
/// # fn main() -> Result<(), prometheus::Error> {
/// let registry = Registry::new();
/// let metrics = PrometheusApiMetrics::new(&registry)?;
///
/// metrics.record_in_flight_delta(Transport::Http, 1);
/// metrics.record_request(
///     Transport::Http,
///     "GET",
///     "/apis/solti.io/v1/tasks",
///     200,
///     12,
/// );
/// metrics.record_in_flight_delta(Transport::Http, -1);
///
/// assert!(!registry.gather().is_empty());
/// # Ok(()) }
/// ```
pub struct PrometheusApiMetrics {
    requests_total: CounterVec,
    duration_seconds: HistogramVec,
    in_flight: GaugeVec,
}

impl PrometheusApiMetrics {
    /// Creates an API metrics backend and registers its collectors.
    ///
    /// The returned backend updates the collectors in `registry`.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the metric group cannot be created or registered.
    /// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let mut metrics = MetricGroup::new();

        let requests_total = metrics.counter_vec(
            "api",
            "requests_total",
            "Total completed API requests",
            &["transport", "method", "path", "status"],
        )?;
        let duration_seconds = metrics.histogram_vec(
            "api",
            "request_duration_seconds",
            "API request duration",
            vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
            &["transport", "method", "path"],
        )?;
        let in_flight = metrics.gauge_vec(
            "api",
            "in_flight_requests",
            "Current in-flight API requests",
            &["transport"],
        )?;
        metrics.register(registry)?;

        Ok(Self {
            requests_total,
            duration_seconds,
            in_flight,
        })
    }
}

impl std::fmt::Debug for PrometheusApiMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusApiMetrics").finish()
    }
}

impl ApiMetricsBackend for PrometheusApiMetrics {
    fn record_request(
        &self,
        transport: Transport,
        method: &str,
        path: &str,
        status: u16,
        duration_ms: u64,
    ) {
        let t = transport.as_label();
        let mut buf = itoa::Buffer::new();
        let s = buf.format(status);
        self.requests_total
            .with_label_values(&[t, method, path, s])
            .inc();
        self.duration_seconds
            .with_label_values(&[t, method, path])
            .observe(ms_to_secs(duration_ms));
    }

    fn record_in_flight_delta(&self, transport: Transport, delta: i64) {
        self.in_flight
            .with_label_values(&[transport.as_label()])
            .add(delta as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{HistogramOpts, Opts};

    #[test]
    fn registration_failure_does_not_leave_a_partial_group() {
        let registry = Registry::new();
        let conflict = HistogramVec::new(
            HistogramOpts::new("request_duration_seconds", "API request duration")
                .namespace("solti")
                .subsystem("api"),
            &["transport", "method", "path"],
        )
        .unwrap();
        registry.register(Box::new(conflict)).unwrap();

        assert!(PrometheusApiMetrics::new(&registry).is_err());

        let requests = CounterVec::new(
            Opts::new("requests_total", "Total completed API requests")
                .namespace("solti")
                .subsystem("api"),
            &["transport", "method", "path", "status"],
        )
        .unwrap();
        assert!(registry.register(Box::new(requests)).is_ok());
    }
}
