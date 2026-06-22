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
//! ```text
//! let api = SupervisorApi::new(sup_cfg, ctrl_cfg, subscribers, router, StateConfig::default()).await?;
//!
//! let task_id = api.submit(&spec).await?;
//! let task    = api.get_task(&task_id);
//! let runs    = api.list_task_runs(&task_id);
//! ```
//!
//! ## Also
//!
//! - [`solti_model::TaskSpec`] input spec submitted via [`SupervisorApi::submit`].
//! - [`taskvisor::Supervisor`] underlying runtime that manages actor lifecycle.
//! - [`solti_runner::RunnerRouter`] picks a runner for each `TaskKind`.

#![forbid(unsafe_code)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::CoreError;

pub mod reasons;

mod map;

mod system;
pub use system::uptime_seconds;

pub mod supervisor;
pub use supervisor::SupervisorApi;

mod state;
pub use state::{StateConfig, TaskState};
