//! Prometheus metrics backend for solti task execution system.
//!
//! This crate provides a [`PrometheusMetrics`] implementation of [`solti_core::MetricsBackend`] that exposes metrics in Prometheus format.
//!
//! ## Example
//! ```rust
//! use std::sync::Arc;
//! use solti_prometheus::PrometheusMetrics;
//! use solti_core::BuildContext;
//! use solti_model::TaskEnv;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create prometheus metrics backend
//! let metrics = PrometheusMetrics::new()?;
//! let metrics_handle = Arc::new(metrics.clone());
//!
//! // Inject into build context
//! let ctx = BuildContext::new(TaskEnv::default(), metrics_handle);
//!
//! // Expose /metrics endpoint (example with custom HTTP server)
//! // let metric_families = metrics.gather();
//! // let encoder = prometheus::TextEncoder::new();
//! // encoder.encode(&metric_families, &mut response_buffer)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Metrics
//! - `solti_tasks_started_total{runner_type}` - Counter
//! - `solti_tasks_completed_total{runner_type, outcome}` - Counter
//! - `solti_task_duration_seconds{runner_type}` - Histogram
//! - `solti_runner_errors_total{runner_type, error_kind}` - Counter
//!
//! ## HTTP Server
//! This crate does NOT provide HTTP server for `/metrics` endpoint.
//! Use your application's existing HTTP framework (axum, warp, etc):
//!
//! ```rust,ignore
//! // Example with axum
//! async fn metrics_handler(
//!     State(metrics): State<Arc<PrometheusMetrics>>
//! ) -> Response {
//!     let families = metrics.gather();
//!     let encoder = prometheus::TextEncoder::new();
//!     let mut buffer = vec![];
//!     encoder.encode(&families, &mut buffer).unwrap();
//!     Response::builder()
//!         .header("Content-Type", encoder.format_type())
//!         .body(buffer.into())
//!         .unwrap()
//! }
//! ```

mod backend;
pub use backend::PrometheusMetrics;

pub use prometheus::{Encoder, Registry, TextEncoder};
