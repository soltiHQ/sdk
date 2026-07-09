//! Resource model.
//!
//! A [`Task`] is the top-level resource. It has metadata, desired spec, and observed status.
//!
//! ```text
//! Task
//!   metadata: ObjectMeta
//!   spec:     TaskSpec
//!   status:   TaskStatus
//! ```

mod spec;
pub use spec::{TaskSpec, TaskSpecBuilder};

pub(crate) mod metadata;
pub use metadata::ObjectMeta;

mod status;
pub use status::TaskStatus;

mod task;
pub use task::Task;

mod run;
pub use run::TaskRun;
