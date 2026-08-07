//! # solti-runner
//!
//! Runner boundary for Solti workloads.
//!
//! A runner converts a [`solti_model::Task`] into a [`taskvisor::TaskRef`].
//! Taskvisor owns execution after that conversion.
//!
//! ## Start Here
//!
//! Use [`Runner`] to implement an execution backend.
//! Use [`RunnerRouter`] to register and select backends.
//! Use [`RunnerCatalog`] to give a composing runner an immutable registration snapshot.
//! Use [`BuildContext`] to inject shared runner dependencies.
//!
//! ## Flow
//!
//! ```text
//! solti_model::Task
//!         ▼
//! RunnerRouter ── GVK + runnerSelector ──▶ Runner
//!         │ allocates RunId                  │ builds
//!         │                                  ▼
//!         └────────────────────────────▶ taskvisor::TaskRef
//! ```
//!
//! The router checks runners in registration order.
//! The first matching runner is selected.
//!
//! [`solti_model::TaskWorkload::Embedded`] bypasses this flow.
//! The router rejects it before runner selection.
//!
//! ## Registration
//!
//! Registration captures an immutable capability snapshot.
//! The snapshot contains the runner name, labels, and supported workload GVKs.
//! Routing and agent capability discovery use the same snapshot.
//! [`RunnerRouter::catalog`] additionally captures the registered runner handles and their routing order for composition.
//!
//! ## Build Contract
//!
//! The router allocates a [`RunId`] for each build.
//! The runner must use [`RunId::name`] as the returned `TaskRef` name.
//! The router validates that name.
//!
//! Building does not start or supervise the task.
//! The returned `TaskRef` may execute more than one attempt.
//!
//! ## Output and Metrics
//!
//! [`OutputPublisher`] creates attempt-scoped [`OutputSink`] values.
//! Runners publish stdout and stderr chunks through those sinks.
//! Channel ownership and subscriptions stay outside this crate.
//!
//! [`MetricsBackend`] records runner setup and cleanup errors.
//! Task lifecycle metrics come from taskvisor events.
//! [`NoOpMetrics`] is used by default.
//!
//! ## Main Types
//!
//! | Area          | Types                                                  |
//! |---------------|--------------------------------------------------------|
//! | Runner plugin | [`Runner`], [`RunnerRouter`], [`RunnerCatalog`]        |
//! | Build data    | [`BuildContext`]                                       |
//! | Output        | [`OutputPublisher`], [`OutputSink`]                    |
//! | Run identity  | [`RunId`], [`make_run_id`]                             |
//! | Metrics       | [`MetricsBackend`], [`MetricsHandle`], [`NoOpMetrics`] |
//! | Metric labels | [`RunnerType`], [`RunnerErrorKind`]                    |
//! | Errors        | [`RouterError`], [`RunnerError`]                       |
//!
//! ## Quick Start
//!
//! Register a runner and build a task:
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
pub use router::{RunnerCatalog, RunnerRouter};

mod id;
pub use id::{RunId, make_run_id};

mod output;
pub use output::{OutputPublisher, OutputPublisherHandle, OutputSink, noop_output_publisher};

pub mod metrics;
pub use metrics::{
    MetricsBackend, MetricsHandle, NoOpMetrics, RunnerErrorKind, RunnerType, noop_metrics,
};
