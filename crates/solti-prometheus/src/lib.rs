//! # solti-prometheus
//!
//! Prometheus metrics for the solti task execution system.
//!
//! This crate provides two complementary metric collectors that together give full observability into the task execution pipeline:
//! - [`PrometheusMetrics`]    - runner-level counters and histograms, injected into runners via [`solti_runner::BuildContext`].
//! - [`PrometheusSubscriber`] - supervision-level gauges and counters, driven by the [`taskvisor`] event stream.
//!
//! Both collectors register into a shared [`prometheus::Registry`] so that a single `metrics` HTTP endpoint can expose the complete picture.
//!
//! ## Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────┐
//!  │                  Shared Registry                        │
//!  │                                                         │
//!  │  PrometheusMetrics          PrometheusSubscriber        │
//!  │  (MetricsBackend)           (taskvisor::Subscribe)      │
//!  │  ├─ solti_runner_*          ├─ solti_sv_*               │
//!  │  │  tasks_started_total     │  tasks_in_flight          │
//!  │  │  tasks_completed_total   │  task_restarts_total      │
//!  │  │  task_duration_seconds   │  task_backoff_*           │
//!  │  │  errors_total            │  task_terminal_total      │
//!  │  │                          │  task_timeouts_total      │
//!  │  │                          │  subscriber_overflow_total│
//!  │  │                          │  subscriber_panicked_total│
//!  │  │                          │                           │
//!  │  │                          ├─ solti_ctrl_* (optional)  │
//!  │  │                          │  submissions_total        │
//!  │  │                          │  rejections_total         │
//!  │  │                          │                           │
//!  └──┼──────────────────────────┼───────────────────────────┘
//!     │                          │
//!     ▼                          ▼
//!   BuildContext              Supervisor
//!   └─► runners call           └─► event bus fans out
//!       record_task_*()            events to on_event()
//! ```
//!
//! ## Metric namespaces
//!
//! | Subsystem                               | Prefix           | Source                         |
//! |-----------------------------------------|------------------|--------------------------------|
//! | [`PrometheusMetrics`]                   | `solti_runner_*` | [`solti_runner::MetricsBackend`] |
//! | [`PrometheusSubscriber`]                | `solti_sv_*`     | [`taskvisor::Subscribe`]       |
//! | `taskvisor::Controller` (feature-gated) | `solti_ctrl_*`   | [`taskvisor::Subscribe`]       |
//!
//! ## Feature flags
//!
//! | Flag         | Default | Effect                                                    |
//! |--------------|---------|-----------------------------------------------------------|
//! | `controller` | off     | Adds `solti_ctrl_*` metrics for controller submit/reject  |
//!
//! ## Example
//!
//! ```text
//! use std::sync::Arc;
//! use prometheus::Registry;
//! use solti_prometheus::{PrometheusMetrics, PrometheusSubscriber};
//!
//! let registry = Arc::new(Registry::new());
//!
//! let metrics = PrometheusMetrics::new_with_registry(registry.clone())?;
//! let metrics_handle = Arc::new(metrics.clone());
//!
//! let subscriber = PrometheusSubscriber::new(registry)?;
//!
//! let ctx = BuildContext::new(RunnerEnv::default(), metrics_handle);
//! let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(subscriber)];
//!
//! async fn metrics_handler(metrics: PrometheusMetrics) -> String {
//!     use solti_prometheus::{Encoder, TextEncoder};
//!     let encoder = TextEncoder::new();
//!     let mut buf = Vec::new();
//!     encoder.encode(&metrics.gather(), &mut buf).unwrap();
//!     String::from_utf8(buf).unwrap()
//! }
//! ```
//!
//! ## Also
//!
//! - [`solti_runner::MetricsBackend`]: trait that [`PrometheusMetrics`] implements.
//! - [`taskvisor::Subscribe`]: trait that [`PrometheusSubscriber`] implements.
//! - See `examples/http-server` for a complete integration example.

mod backend;
pub use backend::PrometheusMetrics;

mod subscriber;
pub use subscriber::PrometheusSubscriber;

pub use prometheus::{Encoder, Registry, TextEncoder};
