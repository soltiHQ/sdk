//! # solti-model
//!
//! Domain model for the solti task execution system.
//!
//! This crate defines the core resource types.
//!
//! ## Architecture
//!
//! ```text
//!  ┌────────────────────────────────────────────────────────┐
//!  │                      Task                              │
//!  │                                                        │
//!  │  ObjectMeta            TaskSpec            TaskStatus  │
//!  │  ├─ id: TaskId         ├─ slot: Slot       ├─ phase    │
//!  │  ├─ generation         ├─ kind: TaskKind   ├─ attempt  │
//!  │  ├─ resource_version   ├─ timeout          └─ error    │
//!  │  ├─ created_at         ├─ restart                      │
//!  │  └─ updated_at         ├─ backoff                      │
//!  │                        ├─ admission                    │
//!  │                        └─ labels                       │
//!  └────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Resource model
//!
//! | Section         | Type             | Responsibility                                       |
//! |-----------------|------------------|------------------------------------------------------|
//! | **metadata**    | [`ObjectMeta`]   | Identity, versioning, timestamps                     |
//! | **spec**        | [`TaskSpec`]     | Desired state: what to run, how to supervise         |
//! | **status**      | [`TaskStatus`]   | Observed state: phase, attempt count, last error     |
//!
//! Slot and labels live in `spec` as the single source of truth.
//! [`Task`] provides convenience accessors ([`Task::slot`], [`Task::labels`]) that delegate to `spec`.
//!
//! ## Versioning
//!
//! [`ObjectMeta`] tracks two counters inspired by K8s:
//! - **`generation`** bumped on spec mutations (user-driven changes)
//! - **`resource_version`** bumped on any change (spec or status)
//!
//! ## Task lifecycle
//!
//! ```text
//!  Pending ──► Running ──► Succeeded
//!                │
//!                ├──► Failed ──► (restart) ──► Running
//!                ├──► Timeout
//!                ├──► Canceled
//!                └──► Exhausted (max retries reached)
//! ```
//!
//! Terminal phases: `Succeeded`, `Failed`, `Timeout`, `Canceled`, `Exhausted`.
//! See [`TaskPhase::is_terminal`].
//!
//! ## Task kinds
//!
//! [`TaskKind`] defines what a task actually runs:
//!
//! | Variant        | Description                                          |
//! |----------------|------------------------------------------------------|
//! | `Subprocess`   | External process (`command`, `args`, `env`, `cwd`)   |
//! | `Wasm`         | WebAssembly module                                   |
//! | `Container`    | OCI container image                                  |
//! | `Embedded`     | Code-defined task (in-process `TaskRef`)             |
//!
//! `Subprocess` tasks go through [`solti_core::RunnerRouter`]; `Embedded` tasks
//! are submitted directly via `SupervisorApi::submit_with_task`.
//!
//! ## Policies
//!
//! | Policy               | Controls                                                 |
//! |----------------------|----------------------------------------------------------|
//! | [`RestartPolicy`]    | When to restart: `Never`, `OnFailure`, `Always`          |
//! | [`BackoffPolicy`]    | Delay between retries: initial, max, factor, jitter      |
//! | [`JitterPolicy`]     | Jitter strategy: `None`, `Full`, `Equal`, `Decorrelated` |
//! | [`AdmissionPolicy`]  | Duplicate handling: `DropIfRunning`, `Replace`, `Queue`  |
//!
//! ## Domain types
//!
//! | Type          | Description                                               |
//! |---------------|-----------------------------------------------------------|
//! | [`Slot`]      | Logical execution lane (newtype over `Arc<str>`)          |
//! | [`TaskId`]    | Unique task identifier (newtype over `Arc<str>`)          |
//! | [`Timeout`]   | Per-attempt timeout in milliseconds                       |
//! | [`Labels`]    | Key-value metadata for routing and filtering              |
//! | [`TaskEnv`]   | Ordered environment variables for task execution          |
//! | [`Flag`]      | Boolean toggle with `enabled()`/`disabled()` constructors |
//! | [`TaskQuery`] | Builder for filtered, paginated task listing              |
//! | [`TaskPage`]  | Paginated query result                                    |
//!
//! ## Example
//!
//! ```rust
//! use solti_model::{
//!     AdmissionPolicy, BackoffPolicy, JitterPolicy, Labels,
//!     RestartPolicy, SubprocessMode, Task, TaskKind, TaskPhase, TaskSpec,
//! };
//!
//! // 1) Define a task spec
//! let spec = TaskSpec {
//!     slot: "my-worker".into(),
//!     kind: TaskKind::Subprocess {
//!         mode: SubprocessMode::Command {
//!             command: "echo".into(),
//!             args: vec!["hello".into()],
//!         },
//!         env: Default::default(),
//!         cwd: None,
//!         fail_on_non_zero: Default::default(),
//!     },
//!     timeout: 5_000_u64.into(),
//!     restart: RestartPolicy::OnFailure,
//!     backoff: BackoffPolicy {
//!         jitter: JitterPolicy::Equal,
//!         first_ms: 1_000,
//!         max_ms: 30_000,
//!         factor: 2.0,
//!     },
//!     admission: AdmissionPolicy::DropIfRunning,
//!     runner_selector: None,
//!     labels: Labels::new(),
//! };
//!
//! // 2) Validate before submission
//! spec.validate().expect("spec should be valid");
//!
//! // 3) Create a task resource (normally done by the supervisor)
//! let task = Task::new("task-001".into(), spec);
//! assert_eq!(task.slot(), "my-worker");
//! assert_eq!(*task.phase(), TaskPhase::Pending);
//! assert_eq!(task.metadata.generation, 1);
//! ```

mod domain;
pub use domain::{
    AdmissionPolicy, BackoffPolicy, Flag, JitterPolicy, KeyValue, Labels, LabelsIter,
    RestartPolicy, RunnerEnv, RunnerSelector, Runtime, SelectorOperator, SelectorRequirement, Slot,
    SubprocessMode, TaskEnv, TaskId, TaskKind, TaskPage, TaskPhase, TaskQuery, Timeout, merge_env,
};

mod resource;
pub use resource::{ObjectMeta, Task, TaskSpec, TaskStatus};

mod error;
pub use error::{ModelError, ModelResult};
