//! Query and pagination types.
//!
//! ```text
//! TaskQuery -> state query -> TaskPage<T>
//!
//! status filters use OR semantics.
//! an empty status filter matches all phases.
//! ```

mod task;
pub use task::{DEFAULT_LIMIT, MAX_LIMIT, TaskPage, TaskQuery};
