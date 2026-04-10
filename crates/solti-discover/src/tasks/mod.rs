//! # Embedded discovery tasks.
//!
//! Each function returns `(TaskRef, TaskSpec)` ready for `SupervisorApi::submit_with_task`.

mod sync;
pub use sync::sync;
