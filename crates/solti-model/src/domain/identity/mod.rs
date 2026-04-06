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

mod agent;
pub use agent::AgentId;

mod task;
pub use task::TaskId;

mod slot;
pub use slot::Slot;
