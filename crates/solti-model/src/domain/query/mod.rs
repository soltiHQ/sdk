//! # Task queries
//!
//! ```text
//! TaskFilter -> TaskQuery -> state query -> TaskPage<T>
//! TaskRunQuery ───────────────► TaskRunPage
//! ```
//!
//! Phase filters use OR semantics.
//! Filter groups use AND semantics.
//! An empty phase filter matches every phase.

mod run;
pub use run::{
    DEFAULT_TASK_RUN_LIMIT, MAX_TASK_RUN_LIMIT, MAX_TASK_RUN_PAGE_ITEM_BYTES, TaskRunContinuation,
    TaskRunPage, TaskRunQuery,
};

mod task;
pub use task::{
    DEFAULT_LIMIT, MAX_LIMIT, MAX_TASK_PAGE_ITEM_BYTES, TaskContinuation, TaskFilter, TaskPage,
    TaskQuery, TaskWatchEvent,
};
