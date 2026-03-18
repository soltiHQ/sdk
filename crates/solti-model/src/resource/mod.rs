//! K8s-style resource model.
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────┐
//!  │                      Task                           │
//!  │                                                     │
//!  │  ObjectMeta            TaskSpec          TaskStatus  │
//!  │  ├─ id: TaskId         ├─ slot            ├─ phase   │
//!  │  ├─ generation         ├─ kind            ├─ attempt │
//!  │  ├─ resource_version   ├─ timeout         ├─ error   │
//!  │  ├─ created_at         ├─ restart         └─ exit_code│
//!  │  └─ updated_at         ├─ backoff                    │
//!  │                        ├─ admission                  │
//!  │                        ├─ runner_selector            │
//!  │                        └─ labels                     │
//!  └─────────────────────────────────────────────────────┘
//! ```
//!
//! | Type           | Role                                                |
//! |----------------|-----------------------------------------------------|
//! | [`Task`]       | Top-level resource = metadata + spec + status       |
//! | [`ObjectMeta`] | Identity, versioning, timestamps                    |
//! | [`TaskSpec`]   | Desired state — what to run, how to supervise       |
//! | [`TaskStatus`] | Observed state — phase, attempt count, last error   |

mod metadata;
pub use metadata::ObjectMeta;

mod spec;
pub use spec::TaskSpec;

mod status;
pub use status::TaskStatus;

mod task;
pub use task::Task;
