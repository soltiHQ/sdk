//! # solti-runner
//!
//! Runner boundary for Solti workloads.
//!
//! A runner converts a [`solti_model::Task`] into a [`taskvisor::TaskRef`].
//! The router returns a [`BuiltTask`] that keeps its [`RunId`] beside that task.
//! Taskvisor owns execution after construction.
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
//!         │ allocates RunId                  │ builds TaskRef
//!         │                                  ▼
//!         └────────────────────────────▶ BuiltTask { RunId, TaskRef }
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
//! It passes the ID to the selected runner and returns the same ID with the
//! executable task in [`BuiltTask`].
//!
//! Building is asynchronous and cancellation-aware.
//! It does not start or supervise the task.
//! The task inside [`BuiltTask`] may execute more than one attempt.
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
//! | Build result  | [`BuiltTask`], [`RunId`]                               |
//! | Build data    | [`BuildContext`], [`BuildCancellation`], [`BuildScope`] |
//! | Admission     | [`RunnerBuildAdmission`], [`AdmittedBuild`]             |
//! | Build owner   | [`BuildCancellationHandle`]                            |
//! | Output        | [`OutputPublisher`], [`OutputSink`]                    |
//! | Run allocator | [`make_run_id`]                                        |
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
//! # use solti_runner::{BuildContext, BuiltTask, RunId, Runner, RunnerError};
//! # use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
//! # struct MyRunner;
//! # #[solti_runner::async_trait]
//! # impl Runner for MyRunner {
//! #     fn name(&self) -> &str { "my-runner" }
//! #     fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
//! #         vec![WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").expect("built-in workload GVK")]
//! #     }
//! #     async fn build_task(&self, _task: &Task, _run_id: &RunId, _ctx: &BuildContext, _cancellation: &solti_runner::BuildCancellation, _scope: &mut solti_runner::BuildScope) -> Result<TaskRef, RunnerError> {
//! #         Ok(TaskFn::arc(|_ctx: TaskContext| async move { Ok::<(), TaskError>(()) }))
//! #     }
//! # }
//! # async fn demo(resource: &Task) -> Result<BuiltTask, Box<dyn std::error::Error>> {
//! let mut router = RunnerRouter::new();
//! router.register(Arc::new(MyRunner))?;
//!
//! let built = router.build(resource).await?;
//! # Ok(built)
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

mod cancellation;
pub use cancellation::{BuildCancellation, BuildCancellationHandle};

mod admission;
pub use admission::{BuildAdmissionConfigError, BuildScope, RunnerBuildAdmission};

/// Attribute used to implement async SDK traits without an additional dependency.
pub use async_trait::async_trait;

mod error;
pub use error::{RouterError, RunnerError};

mod context;
pub use context::BuildContext;

mod environment;
pub use environment::{RunnerEnv, merge_env};

mod router;
pub use router::{AdmittedBuild, BuiltTask, RunnerCatalog, RunnerRouter};

mod id;
pub use id::{RunId, make_run_id};

mod output;
pub use output::{
    OutputChunkRef, OutputPublisher, OutputPublisherHandle, OutputSink, noop_output_publisher,
};

pub mod metrics;
pub use metrics::{
    MetricsBackend, MetricsHandle, NoOpMetrics, RunnerErrorKind, RunnerType, noop_metrics,
};
