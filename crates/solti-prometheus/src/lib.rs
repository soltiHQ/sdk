//! # solti-prometheus
//!
//! Prometheus metrics for Solti agents.
//!
//! This crate gives Solti runners, supervisors, APIs, and discovery tasks one shared Prometheus registry.
//! Use one registry for all collectors, then expose it through your own server or through the optional supervised `/metrics` server.
//!
//! ## Core Model
//!
//! ```text
//! Shared prometheus::Registry
//!   |
//!   |-- PrometheusMetrics          -> solti_runner_*
//!   |-- PrometheusSubscriber       -> solti_sv_* and solti_ctrl_*
//!   |-- PrometheusApiMetrics       -> solti_api_*                  (feature: api)
//!   |-- PrometheusDiscoverMetrics  -> solti_discover_*             (feature: discover)
//!   |-- register_process_collector -> process_*                    (feature: process)
//!   |-- register_build_info        -> solti_build_info
//!   |
//!   v
//! /metrics text endpoint
//! ```
//!
//! ## Main Types
//!
//! | Area              | Types                                            |
//! |-------------------|--------------------------------------------------|
//! | Runner metrics    | [`PrometheusMetrics`]                            |
//! | Taskvisor metrics | [`PrometheusSubscriber`]                         |
//! | Build info        | [`register_build_info`]                          |
//! | Process metrics   | [`register_process_collector`]                   |
//! | Shared registry   | [`Registry`]                                     |
//! | API metrics       | `PrometheusApiMetrics` (feature `api`)           |
//! | Discovery metrics | `PrometheusDiscoverMetrics` (feature `discover`) |
//! | `/metrics` server | `server` (feature `server`)                      |
//! | State snapshot    | `PrometheusStateCollector` (feature `state`)     |
//!
//! ## Quick Start
//!
//! ```rust
//! use std::sync::Arc;
//!
//! use solti_prometheus::{
//!     PrometheusMetrics, PrometheusSubscriber, Registry, register_build_info,
//! };
//! use solti_runner::{MetricOutcome, MetricsBackend, RunnerType};
//! use taskvisor::{Event, EventKind, Subscribe};
//!
//! # fn main() -> Result<(), prometheus::Error> {
//! let registry = Arc::new(Registry::new());
//!
//! let runner_metrics = PrometheusMetrics::new(registry.clone())?;
//! runner_metrics.record_task_started(RunnerType::Subprocess);
//! runner_metrics.record_task_completed(
//!     RunnerType::Subprocess,
//!     MetricOutcome::Success,
//!     42,
//! );
//!
//! let supervisor_metrics = PrometheusSubscriber::new(registry.clone())?;
//! supervisor_metrics.on_event(&Event::new(EventKind::AttemptStarting).with_attempt(1));
//!
//! register_build_info(&registry, &[("version", env!("CARGO_PKG_VERSION"))])?;
//!
//! assert!(!registry.gather().is_empty());
//! # Ok(()) }
//! ```
//!
//! ## Metric Groups
//!
//! - `solti_runner_*`: task execution metrics from `solti-runner`.
//! - `solti_sv_*`: taskvisor supervision metrics.
//! - `solti_ctrl_*`: taskvisor controller metrics.
//! - `solti_api_*`: HTTP/gRPC API metrics, behind feature `api`.
//! - `solti_discover_*`: discovery heartbeat metrics, behind feature `discover`.
//! - `process_*`: standard process metrics, behind feature `process`.
//! - `solti_build_info`: build identity labels.
//!
//! ## Notes
//!
//! All labels are designed to stay low-cardinality.
//! Durations passed in milliseconds are converted to seconds before histogram observation.
//! Register each collector only once per registry.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "state",
    feature = "taskvisor"
))]
mod register;

#[cfg(feature = "taskvisor")]
mod subscriber;
#[cfg(feature = "taskvisor")]
pub use subscriber::{DEFAULT_QUEUE_CAPACITY, PrometheusSubscriber};

mod process;
pub use process::register_process_collector;

#[cfg(feature = "runner")]
mod backend;
#[cfg(feature = "runner")]
pub use backend::PrometheusMetrics;

mod info;
pub use info::register_build_info;

#[cfg(feature = "discover")]
mod discover;
#[cfg(feature = "discover")]
pub use discover::PrometheusDiscoverMetrics;

#[cfg(feature = "api")]
mod api;
#[cfg(feature = "api")]
pub use api::PrometheusApiMetrics;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::{METRICS_SERVER_SLOT, server};

#[cfg(feature = "state")]
mod state;
#[cfg(feature = "state")]
pub use state::PrometheusStateCollector;

pub use prometheus::Registry;
