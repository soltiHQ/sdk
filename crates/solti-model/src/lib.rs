//! # solti-model
//!
//! Domain model for the solti task execution system.
//!
//! This crate defines the core resource types.
//!
//! ## Architecture
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────────┐
//!  │                      Task                                │
//!  │                                                          │
//!  │  ObjectMeta            TaskSpec            TaskStatus    │
//!  │  ├─ id: TaskId         ├─ slot: Slot       ├─ phase      │
//!  │  ├─ resource_version   ├─ kind: TaskKind   ├─ attempt    │
//!  │  ├─ created_at         ├─ timeout          ├─ exit_code  │
//!  │  └─ updated_at         ├─ restart          └─ error      │
//!  │                        ├─ backoff                        │
//!  │                        ├─ admission                      │
//!  │                        ├─ runner_selector                │
//!  │                        └─ labels                         │
//!  └──────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Resource model
//!
//! | Section         | Type             | Responsibility                                                  |
//! |-----------------|------------------|-----------------------------------------------------------------|
//! | **metadata**    | [`ObjectMeta`]   | Identity, versioning, timestamps                                |
//! | **status**      | [`TaskStatus`]   | Observed state: phase, attempt count, exit code, last error     |
//! | **spec**        | [`TaskSpec`]     | Desired state (private fields; build via [`TaskSpec::builder`]) |
//!
//! Slot and labels live in `spec` as the single source of truth.
//! [`Task`] provides convenience accessors ([`Task::slot`], [`Task::labels`]) that delegate to `spec`.
//!
//! ## Versioning
//!
//! [`ObjectMeta::resource_version`] is a monotonic counter bumped on every change
//! (spec or status) for optimistic concurrency.
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
//! `Subprocess` tasks go through `solti_runner::RunnerRouter`; `Embedded` tasks are submitted directly via `SupervisorApi::submit_with_task`.
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
//! ## Construction
//!
//! [`TaskSpec`] fields are private; construct via [`TaskSpec::builder`]:
//!
//! ```text
//! let spec = TaskSpec::builder("my-slot", kind, 5_000u64)
//!     .restart(RestartPolicy::OnFailure)
//!     .build()?;
//! ```
//!
//! See [`TaskSpecBuilder`] for the full API.
//!
//! ## Also
//!
//! - `solti-runner` consumes [`TaskSpec`] and [`TaskKind`] to build executable tasks.
//! - `solti-core` manages [`Task`] lifecycle and state transitions.
//! - `solti-api` serializes/deserializes model types over gRPC and HTTP.
//!
//! ## Domain types
//!
//! | Type               | Description                                               |
//! |--------------------|-----------------------------------------------------------|
//! | [`Slot`]           | Logical execution lane (newtype over `Arc<str>`)          |
//! | [`TaskId`]         | Unique task identifier (newtype over `Arc<str>`)          |
//! | [`Timeout`]        | Per-attempt timeout in milliseconds                       |
//! | [`Labels`]         | Key-value metadata for routing and filtering              |
//! | [`TaskEnv`]        | Ordered environment variables for task execution          |
//! | [`Flag`]           | Boolean toggle with `enabled()`/`disabled()` constructors |
//! | [`TaskQuery`]      | Builder for filtered, paginated task listing              |
//! | [`TaskPage`]       | Paginated query result                                    |
//! | [`TaskSpecBuilder`]| Validated builder for [`TaskSpec`]                        |
//!
//! ## Example
//!
//! ```rust
//! use solti_model::{
//!     BackoffPolicy, JitterPolicy,
//!     RestartPolicy, SubprocessMode, SubprocessSpec, Task, TaskKind, TaskPhase, TaskSpec,
//! };
//!
//! // 1) Build a task spec via the builder
//! let spec = TaskSpec::builder(
//!     "my-worker",
//!     TaskKind::Subprocess(SubprocessSpec {
//!         mode: SubprocessMode::Command {
//!             command: "echo".into(),
//!             args: vec!["hello".into()],
//!         },
//!         env: Default::default(),
//!         cwd: None,
//!         fail_on_non_zero: Default::default(),
//!     }),
//!     5_000u64,
//! )
//! .restart(RestartPolicy::OnFailure)
//! .backoff(BackoffPolicy {
//!     jitter: JitterPolicy::Equal,
//!     first_ms: 1_000,
//!     max_ms: 30_000,
//!     factor: 2.0,
//! })
//! .build()
//! .expect("spec should be valid");
//!
//! // 2) Validate at submit boundary (checks business rules like no Embedded)
//! spec.validate().expect("spec should pass submit validation");
//!
//! // 3) Create a task resource (normally done by the supervisor)
//! let task = Task::new("task-001".into(), spec);
//! assert_eq!(task.slot(), "my-worker");
//! assert_eq!(*task.phase(), TaskPhase::Pending);
//! assert_eq!(task.metadata().resource_version, 1);
//! ```

#![forbid(unsafe_code)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod domain;
pub use domain::{
    AGENT_ID_MAX_LEN, AdmissionPolicy, AgentId, BackoffPolicy, ContainerSpec, DEFAULT_LIMIT, Flag,
    JitterPolicy, KeyValue, Labels, LabelsIter, MAX_LIMIT, MAX_SCRIPT_BODY_BYTES, OutputChunk,
    OutputEvent, RestartPolicy, RunnerEnv, RunnerSelector, Runtime, SLOT_MAX_LEN, SelectorOperator,
    SelectorRequirement, Slot, StreamKind, SubprocessMode, SubprocessSpec, TASK_ID_MAX_LEN,
    TaskEnv, TaskId, TaskKind, TaskPage, TaskPhase, TaskQuery, Timeout, WasmSpec, merge_env,
};

mod resource;
pub use resource::{ObjectMeta, Task, TaskRun, TaskSpec, TaskSpecBuilder, TaskStatus};

mod error;
pub use error::{ModelError, ModelResult};

mod auth;
pub use auth::Token;
