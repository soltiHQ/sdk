//! Prometheus integrations for the Solti SDK.
//!
//! The crate owns no domain metrics contract. Each adapter implements the
//! contract from its source crate and registers metrics into one shared
//! [`Registry`]. All integrations are disabled by default.
//!
//! ## Integrations
//!
//! | Feature                   | Source              | Public API                          |
//! |---------------------------|---------------------|-------------------------------------|
//! | `runner`                  | `solti-runner`      | `PrometheusRunnerMetrics`           |
//! | `api`                     | `solti-api`         | `PrometheusApiMetrics`              |
//! | `discover`                | `solti-discover`    | `PrometheusDiscoverMetrics`         |
//! | `taskvisor`               | Taskvisor events    | `PrometheusTaskvisorSubscriber`     |
//! | `taskvisor-controller`    | Controller events   | controller metrics on the subscriber|
//! | `state`                   | `solti-core`        | `PrometheusCoreStateCollector`      |
//! | `process`                 | current process     | `register_process_collector`        |
//! | `server`                  | shared registry     | supervised `/metrics` task          |
//!
//! `register_build_info` and [`Registry`] are always available.
//!
//! ## Registry
//!
//! ```rust
//! use solti_prometheus::{Registry, register_build_info};
//!
//! # fn main() -> Result<(), prometheus::Error> {
//! let registry = Registry::new();
//! register_build_info(
//!     &registry,
//!     &[("version", env!("CARGO_PKG_VERSION"))],
//! )?;
//!
//! assert!(!registry.gather().is_empty());
//! # Ok(()) }
//! ```
//!
//! Each adapter registers its collectors as one group. A conflicting group is
//! rejected without leaving part of that group in the registry.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

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
pub use subscriber::{DEFAULT_TASKVISOR_QUEUE_CAPACITY, PrometheusTaskvisorSubscriber};

#[cfg(feature = "process")]
mod process;
#[cfg(feature = "process")]
pub use process::register_process_collector;

#[cfg(feature = "runner")]
mod backend;
#[cfg(feature = "runner")]
pub use backend::PrometheusRunnerMetrics;

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
pub use state::PrometheusCoreStateCollector;

pub use prometheus::Registry;
