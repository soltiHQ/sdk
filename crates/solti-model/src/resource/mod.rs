//! K8s-style resource model.
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────────┐
//!  │                      Task                               │
//!  │                                                         │
//!  │  ObjectMeta            TaskSpec          TaskStatus     │
//!  │  ├─ id: TaskId         ├─ slot            ├─ phase      │
//!  │  ├─ resource_version   ├─ kind            ├─ attempt    │
//!  │  ├─ created_at         ├─ timeout         ├─ error      │
//!  │  └─ updated_at         ├─ restart         └─ exit_code  │
//!  │                        ├─ backoff                       │
//!  │                        ├─ admission                     │
//!  │                        ├─ runner_selector               │
//!  │                        └─ labels                        │
//!  └─────────────────────────────────────────────────────────┘
//! ```
//!
//! | Type                | Role                                                |
//! |---------------------|-----------------------------------------------------|
//! | [`Task`]            | Top-level resource = metadata + spec + status       |
//! | [`ObjectMeta`]      | Identity, versioning, timestamps                    |
//! | [`TaskSpec`]        | Desired state - what to run, how to supervise       |
//! | [`TaskSpecBuilder`] | Validated builder for [`TaskSpec`]                  |
//! | [`TaskRun`]         | Record of a single task execution attempt           |
//! | [`TaskStatus`]      | Observed state - phase, attempt count, last error   |

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
