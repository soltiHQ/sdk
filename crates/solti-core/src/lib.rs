//! # solti-core
//!
//! Desired-state supervisor for the Solti SDK.
//!
//! This crate connects [`solti_model`] resources and [`solti_runner`] backends.
//! [`taskvisor`] executes the resulting tasks.
//! It stores tasks in memory and reconciles their desired state.
//!
//! ## Start Here
//!
//! Use [`SupervisorApi`] to create, apply, read, cancel, and delete tasks.
//! Use [`SupervisorApiBuilder`] to configure the runtime and retention.
//! Use [`TaskState`] for shared read access.
//! Use [`OutputSubscription`] for live task output.
//!
//! ## Flow
//!
//! ```text
//! TaskManifest
//!      │ commit desired state
//!      ▼
//! TaskState
//!      │ reconcile
//!      ▼
//! RunnerRouter or embedded TaskRef
//!      │
//!      ▼
//! Taskvisor
//!      ├── best-effort events ──► status, runs, live output
//!      └── direct outcome ──────► authoritative final state
//! ```
//!
//! Desired state is committed before runner construction and runtime intake.
//! A successful write does not mean that execution has started.
//! The `Reconciled` condition reports the reconciliation result.
//!
//! ## Submission Paths
//!
//! [`SupervisorApi::create_task`] and [`SupervisorApi::apply_task`] route a workload.
//! [`solti_runner::RunnerRouter`] selects the runner by GVK and optional labels.
//!
//! [`SupervisorApi::create_embedded_task`] creates embedded state.
//! [`SupervisorApi::apply_embedded_task`] applies embedded state.
//! Both accept a caller-owned [`taskvisor::TaskRef`].
//! Embedded tasks bypass runner routing.
//!
//! The manifest workload and submission path must agree.
//! Routed methods reject embedded workloads.
//! Embedded methods reject routed workloads.
//!
//! ## Reconciliation
//!
//! Reconciliation uses latest-wins semantics.
//! A stale UID or generation cannot replace the current runtime.
//! Accepted side effects are not rolled back when a newer generation arrives.
//! This crate does not provide staged rollout or availability guarantees.
//!
//! Task state has two runtime inputs.
//! Taskvisor events provide attempt detail.
//! The direct completion outcome provides the final task result.
//! It can finalize a task when a terminal event is lost.
//!
//! Typed outcome and rejection kinds select terminal phases.
//! Free-form reason text remains diagnostic.
//! It is never parsed as schema.
//!
//! ## Collections and Output
//!
//! [`TaskState`] stores current tasks and retained [`solti_model::TaskRun`] values.
//! By default, it admits at most 1024 current tasks.
//! It also retains at most 256 MiB of aggregate TaskManifest bytes by default.
//! Every current task counts, including embedded, pending, running, and terminal
//! tasks.
//! The byte budget measures only compact canonical TaskManifest JSON.
//! The count and TaskManifest byte budgets are independent.
//! A full state rejects writes atomically without eviction or waiting for capacity.
//! Existing applies remain allowed by the count limit. The byte budget allows
//! shrinking and no-op applies, but it rejects growth past the configured limit.
//! [`StateConfig`] can disable either limit.
//! The TaskManifest byte budget does not bound total process memory.
//! Task and TaskRun queries use independent snapshot revisions and journals.
//! Both use count limits and a 4 MiB item budget.
//! An oversized first item is returned alone for native transport measurement.
//! Watches replay retained changes before switching to live updates.
//! By default, one state admits 256 concurrent Task watches and retains at most
//! 64 MiB of aggregate compact Task JSON in their initial and replay buffers.
//!
//! Output is live-only and lossy.
//! A slow consumer receives [`solti_model::OutputEvent::Lagged`].
//! Output history is not persisted.
//! [`OutputConfig`] reserves each task ring from a 256 MiB aggregate retained
//! payload budget by default. A task continues without live output when that
//! budget cannot admit a new ring.
//!
//! ## Main Types
//!
//! | Area           | Types                                                  |
//! |----------------|--------------------------------------------------------|
//! | Runtime API    | [`SupervisorApi`], [`SupervisorApiBuilder`]            |
//! | State          | [`TaskState`], [`TaskWatchSubscription`]               |
//! | Output         | [`OutputConfig`], [`OutputSubscription`]               |
//! | Persistence    | [`TaskStateSink`], [`TaskOutputSink`], [`TaskStateSinkStatus`], [`TaskOutputSinkStatus`] |
//! | Configuration  | [`StateConfig`], [`ReconciliationConfig`], [`ConfigError`] |
//! | Writes         | [`WriteConflict`], [`WritePreconditionViolation`]      |
//! | Errors         | [`CoreError`], [`CollectionError`]                     |
//! | Runner routing | [`solti_runner::RunnerRouter`]                         |
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use solti_core::{CoreError, SupervisorApi};
//! use solti_model::{EmbeddedSpec, TaskManifest, TaskSpec, TaskWorkload};
//! use solti_runner::RunnerRouter;
//! use taskvisor::{TaskContext, TaskError, TaskFn};
//!
//! async fn run() -> Result<(), CoreError> {
//!     let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;
//!
//!     let task_ref = TaskFn::arc(|_ctx: TaskContext| async move {
//!         Ok::<(), TaskError>(())
//!     });
//!     let workload = TaskWorkload::Embedded(EmbeddedSpec::new("cleanup-v1")?);
//!     let spec = TaskSpec::builder("maintenance", workload, 5_000_u64).build()?;
//!     let manifest = TaskManifest::new("cleanup", spec)?;
//!     let name = manifest.name().clone();
//!
//!     api.create_embedded_task(manifest, task_ref).await?;
//!     assert!(api.get_task(&name).is_some());
//!
//!     api.shutdown().await
//! }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::{CoreError, WriteConflict, WritePreconditionViolation};

mod config;
pub use config::{ConfigError, ReconciliationConfig, StateConfig};

mod map;

mod output;
pub use output::{OutputConfig, OutputSubscription};

mod persistence;
pub use persistence::{
    PersistenceConfig, TaskOutputEvent, TaskOutputSink, TaskOutputSinkHandle, TaskOutputSinkStatus,
    TaskStateEvent, TaskStateSink, TaskStateSinkHandle, TaskStateSinkStatus,
};

mod runtime;

mod supervisor;
pub use supervisor::{SupervisorApi, SupervisorApiBuilder};

mod state;
pub use state::{CollectionError, TaskState, TaskWatchSubscription};
