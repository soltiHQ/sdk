//! # API Prometheus metrics (feature `api`).
//!
//! [`PrometheusApiMetrics`] implements [`solti_api::ApiMetricsBackend`].
//! It exposes request counters, duration histograms, and an in-flight gauge for HTTP and gRPC API traffic.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::sync::Arc;

use prometheus::{CounterVec, GaugeVec, HistogramVec, Registry};
use solti_api::{ApiMetricsBackend, Transport};

use crate::register::{Sub, ms_to_secs};

/// Prometheus implementation of [`ApiMetricsBackend`].
///
/// ## Metrics (`solti_api_*`)
///
/// | Metric                               | Type      | Labels                                   | Description              |
/// |--------------------------------------|-----------|------------------------------------------|--------------------------|
/// | `solti_api_requests_total`           | Counter   | `transport`, `method`, `path`, `status`  | Completed requests       |
/// | `solti_api_request_duration_seconds` | Histogram | `transport`, `method`, `path`            | Request duration         |
/// | `solti_api_in_flight_requests`       | Gauge     | `transport`                              | In-flight request count  |
///
/// ## Cardinality
///
/// `path` is a templated route for HTTP, such as `/apis/solti.io/v1/tasks/{name}`.
/// For gRPC it is the full method path, such as `/solti.task.v1.TaskService/SubmitTask`.
///
/// In both cases the set is bounded by the proto/api definition.
pub struct PrometheusApiMetrics {
    requests_total: CounterVec,
    duration_seconds: HistogramVec,
    in_flight: GaugeVec,
}

impl PrometheusApiMetrics {
    /// Register all API metrics into `registry`.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use solti_api::{ApiMetricsBackend, Transport};
    /// use solti_prometheus::{PrometheusApiMetrics, Registry};
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let registry = Arc::new(Registry::new());
    /// let metrics = PrometheusApiMetrics::new(registry.clone())?;
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
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let r = Sub::new(&registry, "api");

        let requests_total = r.counter_vec(
            "requests_total",
            "Total completed API requests",
            &["transport", "method", "path", "status"],
        )?;
        let duration_seconds = r.histogram_vec(
            "request_duration_seconds",
            "API request duration",
            vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
            &["transport", "method", "path"],
        )?;
        let in_flight = r.gauge_vec(
            "in_flight_requests",
            "Current in-flight API requests",
            &["transport"],
        )?;

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
