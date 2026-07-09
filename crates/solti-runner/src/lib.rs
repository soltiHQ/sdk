//! # solti-runner
//!
//! Runner plugin interface for Solti tasks.
//!
//! This crate defines how a concrete backend turns a [`solti_model::TaskSpec`] into a [`taskvisor::TaskRef`].
//!
//! Use it when one agent binary needs one or more execution backends: subprocesses, containers, WASM modules, or your own runner.
//!
//! ## Core Model
//!
//! ```text
//! TaskSpec
//!   |
//!   v
//! RunnerRouter
//!   |
//!   | checks Runner::supports(spec)
//!   | checks runner labels, if the spec has a selector
//!   v
//! Runner::build_task(spec, BuildContext)
//!   |
//!   v
//! taskvisor::TaskRef
//! ```
//!
//! A [`Runner`] builds one executable task.
//! A [`RunnerRouter`] chooses a runner for a spec.
//! A [`BuildContext`] carries shared handles such as env, metrics, and output streaming.
//!
//! ## Main Types
//!
//! | Area          | Types                                                  |
//! |---------------|--------------------------------------------------------|
//! | Runner plugin | [`Runner`], [`RunnerRouter`]                           |
//! | Build data    | [`BuildContext`]                                       |
//! | Output        | [`OutputRegistry`], [`OutputSink`]                     |
//! | Run identity  | [`RunId`], [`make_run_id`]                             |
//! | Metrics       | [`MetricsBackend`], [`MetricsHandle`], [`NoOpMetrics`] |
//! | Metric labels | [`RunnerType`], [`MetricOutcome`], [`RunnerErrorKind`] |
//! | Errors        | [`RunnerError`]                                        |
//!
//! ## Quick Start
//!
//! Register a runner and let the router build the task:
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use solti_runner::RunnerRouter;
//!
//! # use solti_model::{TaskKind, TaskSpec};
//! # use solti_runner::{BuildContext, Runner, RunnerError};
//! # use taskvisor::TaskRef;
//! # struct MyRunner;
//! # impl Runner for MyRunner {
//! #     fn name(&self) -> &'static str { "my-runner" }
//! #     fn supports(&self, spec: &TaskSpec) -> bool { matches!(spec.kind(), TaskKind::Subprocess(_)) }
//! #     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
//! # }
//! # fn demo(spec: &TaskSpec) -> Result<TaskRef, RunnerError> {
//! let mut router = RunnerRouter::new();
//! router.register(Arc::new(MyRunner));
//!
//! let task = router.build(spec)?;
//! # Ok(task)
//! # }
//! ```
//!
//! ## Routing
//!
//! Runners are checked in registration order.
//! The first runner that returns `true` from [`Runner::supports`] and matches the optional [`solti_model::RunnerSelector`] is used.
//!
//! [`solti_model::TaskKind::Embedded`] is not routed.
//! Embedded tasks are already built as `TaskRef` values and should be submitted directly through `solti-core`.
//!
//! ## Output and Metrics
//!
//! [`OutputRegistry`] lets runners publish live stdout and stderr lines for HTTP or gRPC log tails.
//!
//! [`MetricsBackend`] is the task execution metrics trait.
//! The default backend is [`NoOpMetrics`]; production agents can use `solti-prometheus`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod runner;
pub use runner::Runner;

mod error;
pub use error::RunnerError;

mod context;
pub use context::BuildContext;

mod router;
pub use router::RunnerRouter;

mod id;
pub use id::{RunId, make_run_id};

mod output;
pub use output::{OutputRegistry, OutputSink};

pub mod metrics;
pub use metrics::{
    MetricOutcome, MetricsBackend, MetricsHandle, NoOpMetrics, RunnerErrorKind, RunnerType,
    noop_metrics,
};
