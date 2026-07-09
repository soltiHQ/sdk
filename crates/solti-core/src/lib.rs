//! # solti-core - orchestration layer.
//!
//! Bridges [`solti-model`](solti_model) (public API types) with the [`taskvisor`] runtime.
//! Provides [`SupervisorApi`]: the main entry point for submitting, querying, and cancelling tasks.
//!
//! ## Architecture
//!
//! ```text
//!  submit(spec) ─► spec.validate() ─► RunnerRouter::build(spec) ─► TaskRef
//!                                                                     │
//!                                       submit_with_task(task, spec) ◄┘
//!                                         ├─ reserve(id, spec)            (provisional state entry)
//!                                         ├─ map policies ─► ControllerSpec
//!                                         └─ handle.submit_and_watch ─► (tv_id, TaskWaiter)
//!                                              ├─ bind_tv(id, tv_id)
//!                                              └─ spawn backstop: TaskWaiter ─► finalize_from_outcome
//!
//!  taskvisor events (lossy bus) ─► StateSubscriber ─► TaskState      (phases + runs)
//!                                                  └─► OutputRegistry (live-tail)
//! ```
//!
//! ## Responsibilities
//!
//! | Component            | What it does                                                 |
//! |----------------------|--------------------------------------------------------------|
//! | [`SupervisorApi`]    | High-level facade: submit, query, cancel, sweep              |
//! | `TaskState`          | In-memory task + run storage (`Arc<RwLock>`)                 |
//! | `StateSubscriber`    | Wires taskvisor events into `TaskState`                      |
//! | `state_sweep`        | Embedded periodic task sweeping expired state (auto-started) |
//! | `map`                | Policy adapter: `solti-model` → `taskvisor` enums            |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use solti_core::taskvisor::{ControllerConfig, SupervisorConfig};
//! use solti_core::{CoreError, RunnerRouter, StateConfig, SupervisorApi};
//! use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec};
//!
//! async fn demo() -> Result<(), CoreError> {
//!     let api = SupervisorApi::new(
//!         SupervisorConfig::default(),
//!         ControllerConfig::default(),
//!         Vec::new(),          // extra event subscribers
//!         RunnerRouter::new(), // register runners for Subprocess/Wasm/Container here
//!         StateConfig::default(),
//!     )
//!     .await?;
//!
//!     let kind = TaskKind::Subprocess(SubprocessSpec::new(
//!         SubprocessMode::Command {
//!             command: "echo".into(),
//!             args: vec!["hello".into()],
//!         },
//!         TaskEnv::default(),
//!         None,
//!         Flag::enabled(),
//!     ));
//!     let spec = TaskSpec::builder("demo-slot", kind, 5_000_u64).build()?;
//!
//!     let task_id = api.submit(&spec).await?;
//!     let _task = api.get_task(&task_id);
//!     let _runs = api.list_task_runs(&task_id);
//!
//!     api.shutdown().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Also
//!
//! - [`solti_model::TaskSpec`] input spec submitted via [`SupervisorApi::submit`].
//! - [`taskvisor::Supervisor`] underlying runtime that manages actor lifecycle.
//! - [`RunnerRouter`] picks a runner for each `TaskKind`.
//! - [`taskvisor`], [`RunnerRouter`] and [`OutputRegistry`] are re-exported so
//!   consumers build against the same runtime versions as this crate.

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
