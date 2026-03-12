//! Prometheus metrics for the solti task execution system.
//!
//! Three metric subsystems under a shared `solti` namespace:
//!
//! | Subsystem                               | Prefix           | Source                         |
//! |-----------------------------------------|------------------|--------------------------------|
//! | [`PrometheusMetrics`]                   | `solti_runner_*` | [`solti_core::MetricsBackend`] |
//! | [`PrometheusSubscriber`]                | `solti_sv_*`     | [`taskvisor::Subscribe`]       |
//! | `taskvisor::Controller` (feature-gated) | `solti_ctrl_*`   | [`taskvisor::Subscribe`]       |
//!
//! All share a single [`Registry`] for a unified `/metrics` endpoint.
//!
//! ## Example
//! ```rust,ignore
//! use std::sync::Arc;
//! use prometheus::Registry;
//! use solti_prometheus::{PrometheusMetrics, PrometheusSubscriber};
//!
//! let registry = Arc::new(Registry::new());
//!
//! // solti_runner_* metrics (injected into BuildContext)
//! let metrics = PrometheusMetrics::new_with_registry(registry.clone())?;
//!
//! // solti_sv_* + solti_ctrl_* metrics (registered as taskvisor subscriber)
//! let subscriber = PrometheusSubscriber::new(registry)?;
//! ```

mod backend;
pub use backend::PrometheusMetrics;

mod subscriber;
pub use subscriber::PrometheusSubscriber;

pub use prometheus::{Encoder, Registry, TextEncoder};
