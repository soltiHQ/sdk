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
//! - [`Slot`]   — logical execution lane, stays the same across submissions.
//! - [`TaskId`] — unique per run, format `{runner}-{slot}-{seq:x}`.

mod slot;
pub use slot::Slot;

mod task;
pub use task::TaskId;
