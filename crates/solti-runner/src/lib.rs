//! # solti-runner
//!
//! Runner plugin interface for Solti tasks.
//!
//! This crate defines how a concrete backend turns a [`solti_model::Task`] into a [`taskvisor::TaskRef`].
//!
//! Use it when one agent binary needs one or more execution backends: subprocesses, containers, WASM modules, or your own runner.
//!
//! ## Core Model
//!
//! ```text
//! Task
//!   |
//!   v
//! RunnerRouter
//!   |
//!   | checks workload GVK
//!   | checks runner labels, if the task spec has a selector
//!   v
//! Runner::build_task(task, RunId, BuildContext)
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
//! | Output        | [`OutputPublisher`], [`OutputSink`]                    |
//! | Run identity  | [`RunId`], [`make_run_id`]                             |
//! | Metrics       | [`MetricsBackend`], [`MetricsHandle`], [`NoOpMetrics`] |
//! | Metric labels | [`RunnerType`], [`RunnerErrorKind`]                    |
//! | Errors        | [`RouterError`], [`RunnerError`]                       |
//!
//! ## Quick Start
//!
//! Register a runner and let the router build the task:
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use solti_runner::RunnerRouter;
//!
//! # use solti_model::{Task, WorkloadTypeMeta, WORKLOAD_API_VERSION};
//! # use solti_runner::{BuildContext, RunId, Runner, RunnerError};
//! # use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
//! # struct MyRunner;
//! # impl Runner for MyRunner {
//! #     fn name(&self) -> &str { "my-runner" }
//! #     fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
//! #         vec![WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").expect("built-in workload GVK")]
//! #     }
//! #     fn build_task(&self, _task: &Task, run_id: &RunId, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
//! #         Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) }))
//! #     }
//! # }
//! # fn demo(resource: &Task) -> Result<TaskRef, Box<dyn std::error::Error>> {
//! let mut router = RunnerRouter::new();
//! router.register(Arc::new(MyRunner))?;
//!
//! let task = router.build(resource)?;
//! # Ok(task)
//! # }
//! ```
//!
//! ## Routing
//!
//! Runners are checked in registration order.
//! The first runner whose GVK and labels match is used.
//!
//! [`solti_model::TaskWorkload::Embedded`] is not routed.
//! Embedded tasks are already built as `TaskRef` values and should be submitted directly through `solti-core`.
//!
//! ## Output and Metrics
//!
//! [`OutputPublisher`] is the narrow producer port used to obtain an
//! attempt-scoped [`OutputSink`]. Channel lifecycle and subscriptions belong to
//! the composition layer, not to runners.
//!
//! [`MetricsBackend`] records runner-specific setup and cleanup errors.
//! Task lifecycle metrics come from taskvisor events.
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
pub use error::{RouterError, RunnerError};

mod context;
pub use context::BuildContext;

mod environment;
pub use environment::{RunnerEnv, merge_env};

mod router;
pub use router::RunnerRouter;

mod id;
pub use id::{RunId, make_run_id};

mod output;
pub use output::{OutputPublisher, OutputPublisherHandle, OutputSink, noop_output_publisher};

pub mod metrics;
pub use metrics::{
    MetricsBackend, MetricsHandle, NoOpMetrics, RunnerErrorKind, RunnerType, noop_metrics,
};
