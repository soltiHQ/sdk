//! Resource identity types.
//!
//! ```text
//!  TaskSpec { slot: "build" }
//!           │
//!           ▼  runner.build()
//!  ┌────────────────────────────────────────────┐
//!  │  Slot: "build"          (stable, logical)  │
//!  │  TaskId: "sub-build-1"  (unique, run)      │
//!  └────────────────────────────────────────────┘
//! ```
//!
//! - [`Slot`]   - logical execution lane, stays the same across submissions.
//! - [`TaskId`] - unique per run, format `{runner}-{slot}-{seq:x}`.
//! - [`AgentId`] - identity of a running agent process.
//!
//! All three share the same `Arc<str>` backing representation and are defined
//! via a single [`arc_str_newtype!`] macro (see `macros.rs`) — they differ only
//! in type name and doc comment.

#[macro_use]
mod macros;

mod agent;
pub use agent::AgentId;

mod slot;
pub use slot::Slot;

mod task;
pub use task::TaskId;
