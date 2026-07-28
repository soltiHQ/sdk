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
pub use condition::{ConditionStatus, TaskCondition, TaskConditionType};

pub(crate) mod metadata;
pub use metadata::{ObjectMeta, Uid};

mod preconditions;
pub use preconditions::WritePreconditions;

mod status;
pub use status::TaskStatus;

mod task;
pub use task::{
    DesiredChange, TASK_API_VERSION, TASK_KIND, Task, TaskManifest, TaskManifestMeta, TypeMeta,
};

mod run;
pub use run::TaskRun;
