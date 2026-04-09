//! # solti-runner
//!
//! Runner plugin interface, routing, and execution metrics for the solti task system.
//!
//! This crate defines how task executors (runners) are declared, selected, and
//! observed. It sits between the domain model ([`solti_model`]) and the
//! orchestration layer (`solti-core`), providing a stable plugin boundary.
//!
//! ## Architecture
//!
//! ```text
//!  TaskSpec ──► RunnerRouter ──► Runner::build_task() ──► TaskRef
//!                  │                    ▲
//!                  │  label matching    │  BuildContext
//!                  │  + supports()      │  (env + metrics)
//!                  ▼                    │
//!              RunnerEntry          MetricsHandle
//!              (runner + labels)    (Arc<dyn MetricsBackend>)
//! ```
//!
//! ## Public API
//!
//! | Item                  | Description                                                  |
//! |-----------------------|--------------------------------------------------------------|
//! | [`Runner`]            | Trait that concrete executors implement                      |
//! | [`RunnerRouter`]      | Selects a runner for a given [`TaskSpec`](solti_model::TaskSpec) by `supports()` + label matching |
//! | [`BuildContext`]      | Shared dependencies injected into runners (env + metrics)    |
//! | [`RunId`]             | Human-readable run identifier (`{runner}-{slot}-{seq}`)      |
//! | [`RunnerError`]       | Error type for runner operations                             |
//! | [`MetricsBackend`]    | Trait for collecting task execution metrics                   |
//! | [`MetricsHandle`]     | `Arc<dyn MetricsBackend>` — cloneable shared handle          |
//! | [`NoOpMetrics`]       | Zero-size backend that compiles to nothing                   |
//! | [`RunnerType`]        | Metric label: `Subprocess`, `Wasm`, `Container`              |
//! | [`TaskOutcome`]       | Metric label: `Success`, `Failure`, `Canceled`, `Timeout`    |
//!
//! ## Runner implementation
//!
//! A runner must implement [`Runner`] and be registered in a [`RunnerRouter`]:
//!
//! ```text
//! use solti_runner::{Runner, RunnerError, BuildContext, RunId};
//! use solti_model::TaskSpec;
//! use taskvisor::TaskRef;
//!
//! struct MyRunner;
//!
//! impl Runner for MyRunner {
//!     fn name(&self) -> &'static str { "my-runner" }
//!
//!     fn supports(&self, spec: &TaskSpec) -> bool {
//!         matches!(spec.kind(), solti_model::TaskKind::Subprocess(_))
//!     }
//!
//!     fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
//!         // build and return a TaskRef
//!         todo!()
//!     }
//! }
//! ```
//!
//! ## Routing
//!
//! [`RunnerRouter`] selects the first registered runner that:
//! 1. returns `true` from [`Runner::supports`] for the given spec, and
//! 2. satisfies the [`RunnerSelector`](solti_model::RunnerSelector) label constraints (if any).
//!
//! ```text
//! use solti_runner::{RunnerRouter, BuildContext};
//!
//! let mut router = RunnerRouter::new();
//! router.register(Arc::new(MyRunner));
//!
//! let task_ref = router.build(&spec)?;
//! ```
//!
//! ## Metrics
//!
//! Metrics are collected via [`MetricsBackend`], injected through [`BuildContext`].
//! The default is [`NoOpMetrics`] (zero-cost). Backends like `solti-prometheus`
//! implement [`MetricsBackend`] for production use.
//!
//! ## Also
//!
//! - [`solti_model`] — domain types consumed by runners ([`TaskSpec`](solti_model::TaskSpec), [`TaskKind`](solti_model::TaskKind)).
//! - [`taskvisor::TaskRef`] — the concrete task handle returned by [`Runner::build_task`].
//! - `solti-exec` — subprocess runner implementation.
//! - `solti-prometheus` — Prometheus [`MetricsBackend`] implementation.

mod error;
pub use error::RunnerError;

mod runner;
pub use runner::Runner;

mod context;
pub use context::BuildContext;

mod id;
pub use id::{RunId, make_run_id};

mod router;
pub use router::RunnerRouter;

pub mod metrics;
pub use metrics::{
    MetricsBackend, MetricsHandle, NoOpMetrics, RunnerType, TaskOutcome, noop_metrics,
};
