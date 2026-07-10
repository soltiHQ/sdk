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
//! use solti_core::taskvisor::{
//!     ControllerConfig, SupervisorConfig, TaskContext, TaskError, TaskFn,
//! };
//! use solti_core::{CoreError, RunnerRouter, StateConfig, SupervisorApi};
//! use solti_model::{RestartPolicy, TaskKind, TaskSpec};
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
//!     let task = TaskFn::arc("embedded-demo", |_ctx: TaskContext| async move {
//!         Ok::<(), TaskError>(())
//!     });
//!     let spec = TaskSpec::builder("embedded", TaskKind::Embedded, 1_000_u64)
//!         .restart(RestartPolicy::Never)
//!         .build()?;
//!
//!     let task_id = api.submit_with_task(task, &spec).await?;
//!     let _task = api.get_task(&task_id);
//!     let _runs = api.list_task_runs(&task_id);
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
//!   -> OutputRegistry streams live output
//! ```
//!
//! `submit_with_task()` is for embedded Rust tasks that already have a
//! `taskvisor::TaskRef`. Use `submit()` when a [`RunnerRouter`] can build the
//! task from its [`solti_model::TaskKind`].
//!
//! ## State
//!
//! Task state is rebuilt from two sources:
//!
//! - taskvisor events, which carry attempt-level detail;
//! - taskvisor completion waiters, which repair terminal state if a final event
//!   was dropped by the best-effort event bus.
//!
//! Terminal phases are sticky. A later actor-level event must not replace a more
//! specific phase such as `Timeout` or `Canceled`.
//!
//! ## Common Types
//!
//! | Type | Role |
//! |------|------|
//! | [`SupervisorApi`] | Main public entry point over the supervisor runtime. |
//! | [`StateConfig`] | Retention settings for tasks and run history. |
//! | [`TaskState`] | Shared in-memory read handle. |
//! | [`CoreError`] | Error type returned by fallible APIs. |
//! | [`RunnerRouter`] | Builds concrete runner tasks from model specs. |
//! | [`OutputRegistry`] | Live-tail output registry used by API streams. |
//!
//! [`taskvisor`], [`RunnerRouter`], and [`OutputRegistry`] are re-exported so
//! host applications use the same runtime versions as this crate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::CoreError;

mod map;

mod system;
pub use system::uptime_seconds;

pub mod supervisor;
pub use supervisor::SupervisorApi;

mod state;
pub use state::{StateConfig, TaskState};

pub use solti_runner::{OutputRegistry, RunnerRouter};
pub use taskvisor;
