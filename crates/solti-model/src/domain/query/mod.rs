//! # Task queries
//!
//! ```text
//! TaskFilter -> TaskQuery -> state query -> TaskPage<T>
//! ```
//!
//! Phase filters use OR semantics.
//! Filter groups use AND semantics.
//! An empty phase filter matches every phase.

mod task;
pub use task::{
    DEFAULT_LIMIT, MAX_LIMIT, TaskContinuation, TaskFilter, TaskPage, TaskQuery, TaskWatchEvent,
};
