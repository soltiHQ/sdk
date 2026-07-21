//! Supervisor layer for the Solti SDK.
//!
//! `solti-core` connects [`solti_model`] task specs, [`solti_runner`] runners,
//! and the [`taskvisor`] runtime.
//!
//! The main type is [`SupervisorApi`]. It lets an application submit tasks,
//! query their state, read run history, cancel tasks, and shut the runtime down.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use solti_core::{CoreError, StateConfig, SupervisorApi};
//! use solti_model::{EmbeddedSpec, RestartPolicy, TaskManifest, TaskSpec, TaskWorkload};
//! use solti_runner::RunnerRouter;
//! use taskvisor::{ControllerConfig, SupervisorConfig, TaskContext, TaskError, TaskFn};
//!
//! async fn demo() -> Result<(), CoreError> {
//!     let api = SupervisorApi::new(
//!         SupervisorConfig::default(),
//!         ControllerConfig::default(),
//!         Vec::new(),
//!         RunnerRouter::new(),
//!         StateConfig::default(),
//!     )
//!     .await?;
//!
//!     let task_ref = TaskFn::arc("embedded-demo", |_ctx: TaskContext| async move {
//!         Ok::<(), TaskError>(())
//!     });
//!     let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1")?);
//!     let spec = TaskSpec::builder("embedded", workload, 1_000_u64)
//!         .restart(RestartPolicy::Never)
//!         .build()?;
//!     let manifest = TaskManifest::new("embedded-demo", spec)?;
//!     let name = manifest.name().clone();
//!
//!     api.create_with_task(manifest, task_ref).await?;
//!     let _task = api.get_task(&name);
//!     let _runs = api.list_task_runs(&name);
//!
//!     api.shutdown().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Main Flow
//!
//! ```text
//! TaskSpec
//!   -> RunnerRouter builds a task
//!   -> taskvisor runs the task
//!   -> TaskState stores phase and run history
//!   -> core-owned output hub streams live output
//! ```
//!
//! [`SupervisorApi::create_with_task`] and [`SupervisorApi::apply_with_task`]
//! accept embedded Rust tasks that already have a `taskvisor::TaskRef`.
//! [`SupervisorApi::create_task`] and [`SupervisorApi::apply_task`] route a
//! resource through [`RunnerRouter`] using its [`solti_model::TaskWorkload`].
//!
//! ## State
//!
//! Task state is rebuilt from two sources:
//!
//! - taskvisor events, which carry attempt-level detail;
//! - direct taskvisor completion outcomes, which repair terminal state if a
//!   final event was dropped by the best-effort event bus.
//!
//! Final phases use typed outcome and rejection categories. Event `reason`
//! text remains diagnostic and is never parsed to choose a phase.
//!
//! `TaskRemoved` normally acts as a FIFO barrier before cleanup so attempt
//! events can update run history first; the direct outcome is the bounded
//! fallback if that best-effort barrier is lost or delayed.
//!
//! The joined outcome reconciles the resource-level disposition. Attempt
//! history remains independent, and a concrete `Timeout` stays more specific
//! than the final task's generic `Exhausted` disposition.
//!
//! ## Common Types
//!
//! | Type | Role |
//! |------|------|
//! | [`SupervisorApi`] | Main public entry point over the supervisor runtime. |
//! | [`StateConfig`] | Retention settings for tasks and run history. |
//! | [`TaskState`] | Shared in-memory read handle. |
//! | [`CoreError`] | Error type returned by fallible APIs. |
//! | [`solti_runner::RunnerRouter`] | Builds concrete runner tasks from model specs. |
//! | [`OutputConfig`] | Per-task live-output ring configuration. |
//! | [`OutputSubscription`] | Consumer-only live-output stream. |
//!
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::CoreError;

mod map;

mod output;
pub use output::{OutputConfig, OutputSubscription};

pub mod supervisor;
pub use supervisor::SupervisorApi;

mod state;
pub use state::{StateConfig, TaskState};
