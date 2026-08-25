//! # solti-model
//!
//! Shared resource model for Solti agents and control planes.
//!
//! This crate defines task resources, workloads, policies, selectors, queries,
//! output events, capabilities, and bearer tokens.
//! It does not execute tasks or own resource storage.
//!
//! ## Start Here
//!
//! Use [`TaskManifest`] for caller-owned desired state.
//! Use [`Task`] for a stored resource with server metadata and status.
//! Use [`TaskSpec`] to describe execution.
//! Use [`TaskWorkload`] to select a built-in or extension workload.
//!
//! ## Resource Flow
//!
//! ```text
//! caller
//!   │ TaskManifest
//!   ▼
//! Task::from_manifest ── generates uid and creationTimestamp
//!   │                   └─ starts generation at 1
//!   ▼
//! Task
//!   ├── metadata: ObjectMeta
//!   ├── spec:     TaskSpec
//!   ├── status:   TaskStatus
//!   │
//!   └── state store assigns resourceVersion
//! ```
//!
//! The model validates values and applies state transitions.
//! Storage, reconciliation, execution, and transport stay in higher layers.
//!
//! ## Features
//!
//! The default `schema` feature implements `schemars::JsonSchema` for resource, workload, selector, capability, and output types.
//! Disable default features when schema generation is not needed.
//! Runtime validation remains authoritative for cross-field and byte-budget rules.
//!
//! ## Quick Start
//!
//! Build a task spec:
//!
//! ```rust
//! use solti_model::{
//!     Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskSpec, TaskWorkload,
//! };
//!
//! let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
//!     SubprocessMode::Command {
//!         command: "echo".into(),
//!         args: vec!["hello".into()],
//!     },
//!     TaskEnv::default(),
//!     None,
//!     Flag::enabled(),
//! ));
//!
//! let spec = TaskSpec::builder("hello", workload, 5_000u64)
//!     .build()
//!     .expect("valid spec");
//!
//! spec.validate().expect("valid spec");
//! assert_eq!(spec.slot().as_str(), "hello");
//! ```
//!
//! Create a stored task resource:
//!
//! ```rust
//! use solti_model::{EmbeddedSpec, Task, TaskPhase, TaskSpec, TaskWorkload};
//!
//! let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
//! let spec = TaskSpec::builder("cleanup", workload, 1_000u64)
//!     .build()
//!     .unwrap();
//!
//! let task = Task::new("embedded-cleanup-1", spec).unwrap();
//!
//! assert_eq!(*task.phase(), TaskPhase::Pending);
//! assert_eq!(task.name().as_str(), "embedded-cleanup-1");
//! ```
//!
//! [`TaskWorkload::Embedded`] is valid in the shared model.
//! API and runner layers apply their own admission rules.
//!
//! ## Resource Model
//!
//! ```text
//! Task
//!   apiVersion, kind
//!   metadata: ObjectMeta
//!   spec:     TaskSpec
//!   status:   TaskStatus
//!
//! TaskSpec
//!   slot, workload, timeout, restart, backoff, admission
//!   max_retries, runner_selector
//!
//! TaskStatus
//!   observed_generation, conditions, phase, attempt, exit_code, error
//! ```
//!
//! [`TaskSpec`] is desired state.
//! [`TaskStatus`] is observed state.
//! [`ObjectMeta`] carries identity, versions, labels, annotations, and timestamps.
//! Task and TaskRun execution diagnostics are UTF-8-safe prefixes bounded by
//! [`MAX_TASK_DIAGNOSTIC_BYTES`].
//!
//! ## Lifecycle
//!
//! ```text
//! Pending ──▶ Running ──▶ Succeeded
//!             ├────────▶ Failed
//!             ├────────▶ Timeout
//!             └────────▶ Canceled
//!
//! Failed | Timeout ── retry budget exhausted ──▶ Exhausted
//! ```
//!
//! Terminal phases are `Succeeded`, `Failed`, `Timeout`, `Canceled`, and `Exhausted`.
//! See [`TaskPhase::is_terminal`].
//!
//! ## Task Workloads
//!
//! [`TaskWorkload`] describes what a task runs:
//!
//! | Kind         | Meaning                | Routed by runner |
//! |--------------|------------------------|------------------|
//! | `Subprocess` | Host command or script | yes              |
//! | `Container`  | OCI image              | yes              |
//! | `Wasm`       | WASI module            | yes              |
//! | `Embedded`   | In-process task        | no               |
//! | `Extension`  | Application-defined    | yes              |
//!
//! Routable variants are consumed by `solti-runner`.
//! Embedded workloads bypass runner routing.
//!
//! ## Selectors
//!
//! [`LabelSelector`] matches runner labels. All requirements are ANDed:
//!
//! ```rust
//! use solti_model::{Labels, LabelSelector, SelectorRequirement};
//!
//! let selector = LabelSelector {
//!     match_labels: {
//!         let mut labels = Labels::new();
//!         labels.insert("zone", "eu");
//!         labels
//!     },
//!     match_expressions: vec![SelectorRequirement::exists("gpu")],
//! };
//!
//! let mut runner = Labels::new();
//! runner.insert("zone", "eu");
//! runner.insert("gpu", "h100");
//!
//! assert!(selector.matches(&runner));
//! ```
//!
//! ## Auth
//!
//! [`Token`] wraps a bearer secret.
//! Its `Debug` output is redacted.
//! [`Token::verify`] uses a constant-time comparison for equal-length values.
//!
//! ## Main Types
//!
//! | Area         | Types                                                                                          |
//! |--------------|------------------------------------------------------------------------------------------------|
//! | Resource     | [`Task`], [`TaskManifest`], [`TaskSpec`], [`TaskStatus`], [`ObjectMeta`], [`TaskRun`]          |
//! | Identity     | [`Slot`], [`TaskId`], [`AgentId`], [`Uid`]                                                     |
//! | Workload     | [`TaskWorkload`], [`ExtensionWorkload`], [`SubprocessSpec`], [`WasmSpec`], [`ContainerSpec`]   |
//! | Policies     | [`RestartPolicy`], [`BackoffPolicy`], [`JitterPolicy`], [`AdmissionPolicy`], [`Timeout`]       |
//! | Selection    | [`Labels`], [`LabelSelector`], [`SelectorRequirement`], [`SelectorOperator`]                   |
//! | Capabilities | [`AgentCapabilities`], [`RunnerCapability`], [`WorkloadTypeMeta`]                              |
//! | Query        | [`TaskQuery`], [`TaskRunQuery`], continuations, pages, [`TaskWatchEvent`]                      |
//! | Output       | [`OutputEvent`], [`OutputChunk`], [`StreamKind`]                                               |
//! | Auth         | [`Token`]                                                                                      |
//! | Errors       | [`ModelError`], [`ModelResult`]                                                                |
//!
//! ## See Also
//!
//! - `solti-runner` consumes [`TaskSpec`] and [`TaskWorkload`] to build executable tasks.
//! - `solti-core` manages [`Task`] lifecycle and state transitions.
//! - `solti-api` serializes model types over gRPC and HTTP.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod domain;
pub use domain::{
    AGENT_ID_MAX_LEN, AdmissionPolicy, AgentCapabilities, AgentId, BackoffPolicy, ContainerSpec,
    DEFAULT_LIMIT, DEFAULT_TASK_RUN_LIMIT, EmbeddedSpec, ExtensionWorkload, Flag, JitterPolicy,
    KeyValue, LabelSelector, Labels, LabelsIter, MAX_LIMIT, MAX_SCRIPT_BODY_BYTES,
    MAX_TASK_PAGE_ITEM_BYTES, MAX_TASK_RUN_LIMIT, MAX_TASK_RUN_PAGE_ITEM_BYTES, OutputChunk,
    OutputEvent, RestartPolicy, RunnerCapability, SLOT_MAX_LEN, SelectorOperator,
    SelectorRequirement, Slot, StreamKind, SubprocessMode, SubprocessSpec, TASK_ID_MAX_LEN,
    TaskContinuation, TaskEnv, TaskFilter, TaskId, TaskPage, TaskPhase, TaskQuery,
    TaskRunContinuation, TaskRunPage, TaskRunQuery, TaskWatchEvent, TaskWorkload, Timeout,
    WORKLOAD_API_VERSION, WasmSpec, WorkloadTypeMeta,
};

mod resource;
pub use resource::{
    Annotations, ConditionStatus, DesiredChange, MAX_TASK_DIAGNOSTIC_BYTES,
    MAX_TASK_MANIFEST_BYTES, ObjectMeta, TASK_API_VERSION, TASK_API_VERSION_MAJOR, TASK_KIND, Task,
    TaskCondition, TaskConditionType, TaskManifest, TaskManifestMeta, TaskRun, TaskSpec,
    TaskSpecBuilder, TaskStatus, TypeMeta, Uid, WritePreconditions,
};

mod error;
pub use error::{ModelError, ModelResult};

mod auth;
pub use auth::Token;

mod validation;

#[cfg(feature = "schema")]
mod schema;
