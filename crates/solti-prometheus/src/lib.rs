//! # solti-prometheus
//!
//! `solti-prometheus` connects Solti metrics contracts and public runtime
//! snapshots to one Prometheus [`Registry`]. It does not define those
//! contracts. Event adapters implement traits from the crates that own the
//! observed behavior; pull collectors read public snapshots during gathering.
//!
//! All integrations are disabled by default.
//! [`Registry`], [`Error`], and [`register_build_info`] are always available.
//!
//! ## Data Flow
//!
//! ```text
//! solti-runner ─────► PrometheusRunnerMetrics ───────────┐
//! solti-api ────────► PrometheusApiMetrics ──────────────┤
//! solti-discover ───► PrometheusDiscoverMetrics ─────────┤
//! subprocess runner ► finalizer status collector ────────┤
//! containerd engine ► local worker status collector ─────┤
//! Taskvisor events ─► PrometheusTaskvisorSubscriber ─────┤
//! solti-core state ─► PrometheusCoreStateCollector ──────┤
//! build / process ──► registration functions ────────────┤
//!                                                        ▼
//!                                               shared Registry
//!                                                        ▼
//!                                                   GET /metrics
//! ```
//!
//! ## Choose an Integration
//!
//! | Feature                | Input              | Public API                       |
//! |------------------------|--------------------|----------------------------------|
//! | `api`                  | API metrics trait  | `PrometheusApiMetrics`           |
//! | `discover`             | Discovery metrics  | `PrometheusDiscoverMetrics`      |
//! | `containerd`           | Containerd engine  | `PrometheusContainerdCollector`  |
//! | `process`              | Current process    | `register_process_collector`     |
//! | `runner`               | Runner metrics     | `PrometheusRunnerMetrics`        |
//! | `server`               | Shared registry    | `server`, `server_with_config`   |
//! | `state`                | `TaskState`        | `PrometheusCoreStateCollector`   |
//! | `subprocess`           | Subprocess runner  | `PrometheusSubprocessFinalizerCollector` |
//! | `taskvisor`            | Taskvisor events   | `PrometheusTaskvisorSubscriber`  |
//! | `taskvisor-controller` | Controller events  | Controller subscriber metrics    |
//!
//! The `full` feature enables every integration in the table.
//!
//! ## Registration
//!
//! Push adapters register their collectors as one group. A descriptor conflict
//! rejects the complete group without leaving part of that group in the
//! registry.
//!
//! Pull-based state, subprocess, and containerd collectors are returned
//! unregistered. Construct them first, then register each one as a
//! [`prometheus::core::Collector`].
//!
//! ## Quick Start
//!
//! ```rust
//! use solti_prometheus::{Error, Registry, register_build_info};
//!
//! # fn main() -> Result<(), Error> {
//! let registry = Registry::new();
//! register_build_info(
//!     &registry,
//!     &[("version", env!("CARGO_PKG_VERSION"))],
//! )?;
//!
//! assert!(!registry.gather().is_empty());
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(all(doctest, feature = "full"))]
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
#[cfg_attr(docsrs, doc(cfg(feature = "taskvisor")))]
pub use subscriber::{DEFAULT_TASKVISOR_QUEUE_CAPACITY, PrometheusTaskvisorSubscriber};

#[cfg(feature = "process")]
mod process;
#[cfg(feature = "process")]
#[cfg_attr(docsrs, doc(cfg(feature = "process")))]
pub use process::register_process_collector;

#[cfg(feature = "runner")]
mod backend;
#[cfg(feature = "runner")]
#[cfg_attr(docsrs, doc(cfg(feature = "runner")))]
pub use backend::PrometheusRunnerMetrics;

mod info;
pub use info::register_build_info;

#[cfg(feature = "discover")]
mod discover;
#[cfg(feature = "discover")]
#[cfg_attr(docsrs, doc(cfg(feature = "discover")))]
pub use discover::PrometheusDiscoverMetrics;

#[cfg(feature = "api")]
mod api;
#[cfg(feature = "api")]
#[cfg_attr(docsrs, doc(cfg(feature = "api")))]
pub use api::PrometheusApiMetrics;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub use server::{
    METRICS_SERVER_SLOT, MetricsServerConfig, MetricsServerConfigError, server, server_with_config,
};

#[cfg(feature = "state")]
mod state;
#[cfg(feature = "state")]
#[cfg_attr(docsrs, doc(cfg(feature = "state")))]
pub use state::PrometheusCoreStateCollector;

pub use prometheus::{Error, Registry};

#[cfg(feature = "subprocess")]
mod subprocess;
#[cfg(feature = "subprocess")]
#[cfg_attr(docsrs, doc(cfg(feature = "subprocess")))]
pub use subprocess::PrometheusSubprocessFinalizerCollector;

#[cfg(feature = "containerd")]
mod containerd;
#[cfg(feature = "containerd")]
#[cfg_attr(docsrs, doc(cfg(feature = "containerd")))]
pub use containerd::PrometheusContainerdCollector;
