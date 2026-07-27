//! Query and pagination types.
//!
//! ```text
//! TaskFilter -> TaskQuery -> state query -> TaskPage<T>
//!
//! Phase filters use OR semantics.
//! An empty phase filter matches all phases.
//! ```

mod task;
pub use task::{
    DEFAULT_LIMIT, MAX_LIMIT, TaskContinuation, TaskFilter, TaskPage, TaskQuery, TaskWatchEvent,
};
