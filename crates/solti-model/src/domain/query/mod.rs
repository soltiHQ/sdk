//! Query and pagination types.
//!
//! ```text
//!  TaskQuery
//!  ┌───────────────────────────────────────────┐
//!  │  status: Vec<TaskPhase>   OR semantics    │
//!  │  slot:   Option<Slot>     filter by lane  │
//!  │  limit:  usize            page size       │
//!  │  offset: usize            skip N          │
//!  └────────────────┬──────────────────────────┘
//!                   ▼
//!  TaskPage<T> { items: Vec<T>, total: usize }
//! ```

mod task;
pub use task::{DEFAULT_LIMIT, MAX_LIMIT, TaskPage, TaskQuery};
