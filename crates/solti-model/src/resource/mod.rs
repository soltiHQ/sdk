//! # Resource model
//!
//! [`TaskManifest`] is caller-owned desired state.
//! [`Task`] adds server-owned metadata and observed status.
//!
//! ```text
//! Task
//!   metadata: ObjectMeta
//!   spec:     TaskSpec
//!   status:   TaskStatus
//! ```

mod spec;
pub use spec::{TaskSpec, TaskSpecBuilder};

mod annotations;
pub use annotations::Annotations;

mod condition;
#[cfg(feature = "schema")]
pub(crate) use condition::{CONDITION_REASON_MAX_BYTES, CONDITION_TYPE_MAX_BYTES};
pub use condition::{ConditionStatus, TaskCondition, TaskConditionType};

pub(crate) mod metadata;
pub use metadata::{ObjectMeta, Uid};

mod preconditions;
pub use preconditions::WritePreconditions;

mod status;
pub use status::{MAX_TASK_DIAGNOSTIC_BYTES, TaskStatus};

mod task;
pub use task::{
    DesiredChange, MAX_TASK_MANIFEST_BYTES, TASK_API_VERSION, TASK_API_VERSION_MAJOR, TASK_KIND,
    Task, TaskManifest, TaskManifestMeta, TypeMeta,
};

mod run;
pub use run::TaskRun;
