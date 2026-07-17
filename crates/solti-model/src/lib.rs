//! Shared task model for Solti agents and control planes.
//!
//! `solti-model` contains the data types that all Solti crates speak.
//! It defines task specs, task status, ids, policies, runner selectors, environment variables, output events, and agent tokens.
//!
//! Use it when you build a Solti API, runner, supervisor, control plane, or tool that reads or writes task data.
//!
//! ## Quick Start
//!
//! Build a task spec and validate it at the submit boundary:
//!
//! ```rust
//! use solti_model::{
//!     Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec,
//! };
//!
//! let kind = TaskKind::Subprocess(SubprocessSpec::new(
//!     SubprocessMode::Command {
//!         command: "echo".into(),
//!         args: vec!["hello".into()],
//!     },
//!     TaskEnv::default(),
//!     None,
//!     Flag::enabled(),
//! ));
//!
//! let spec = TaskSpec::builder("hello", kind, 5_000u64)
//!     .build()
//!     .expect("valid spec");
//!
//! spec.validate().expect("submittable spec");
//! assert_eq!(spec.slot().as_str(), "hello");
//! ```
//!
//! Create a task resource from a spec:
//!
//! ```rust
//! use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};
//!
//! let spec = TaskSpec::builder("cleanup", TaskKind::Embedded, 1_000u64)
//!     .build()
//!     .unwrap();
//!
//! let task = Task::new(TaskId::from("embedded-cleanup-1"), spec);
//!
//! assert_eq!(*task.phase(), TaskPhase::Pending);
//! assert_eq!(task.id().as_str(), "embedded-cleanup-1");
//! ```
//!
//! `TaskKind::Embedded` is valid as model data, but [`TaskSpec::validate`] rejects it for runner-based submit.
//! Embedded tasks must be submitted with a real `TaskRef`.
//!
//! ## What Ships
//!
//! | Area        | Main Types                                                                                         |
//! |-------------|----------------------------------------------------------------------------------------------------|
//! | Resource    | [`Task`], [`TaskSpec`], [`TaskStatus`], [`ObjectMeta`], [`TaskRun`]                                |
//! | Identity    | [`Slot`], [`TaskId`], [`AgentId`]                                                                  |
//! | Execution   | [`TaskKind`], [`SubprocessSpec`], [`SubprocessMode`], [`WasmSpec`], [`ContainerSpec`], [`Runtime`] |
//! | Policies    | [`RestartPolicy`], [`BackoffPolicy`], [`JitterPolicy`], [`AdmissionPolicy`], [`Timeout`]           |
//! | Routing     | [`Labels`], [`RunnerSelector`], [`SelectorRequirement`], [`SelectorOperator`]                      |
//! | Environment | [`TaskEnv`], [`RunnerEnv`], [`KeyValue`], [`merge_env`]                                            |
//! | Query       | [`TaskQuery`], [`TaskPage`]                                                                        |
//! | Output      | [`OutputEvent`], [`OutputChunk`], [`StreamKind`]                                                   |
//! | Auth        | [`Token`]                                                                                          |
//! | Errors      | [`ModelError`], [`ModelResult`]                                                                    |
//!
//! ## Core Model
//!
//! ```text
//! Task
//!   metadata: ObjectMeta
//!   spec:     TaskSpec
//!   status:   TaskStatus
//!
//! TaskSpec
//!   slot, kind, timeout, restart, backoff, admission
//!   max_retries, runner_selector, labels
//!
//! TaskStatus
//!   phase, attempt, exit_code, error
//! ```
//!
//! [`TaskSpec`] says what should run. [`TaskStatus`] says what happened.
//! [`ObjectMeta`] carries identity, version, and timestamps.
//!
//! ## Lifecycle
//!
//! ```text
//! Pending -> Running -> attempt outcome: Succeeded | Failed | Timeout
//!               ^                              |
//!               +--------- restart policy -----+
//!                                              |
//!                                              +-> lifecycle disposition:
//!                                                  Succeeded | Failed | Timeout |
//!                                                  Exhausted | Canceled
//! ```
//!
//! Terminal phases are `Succeeded`, `Failed`, `Timeout`, `Canceled`, and `Exhausted`.
//! A terminal attempt phase may still be followed by another attempt according
//! to the restart policy.
//! See [`TaskPhase::is_terminal`].
//!
//! ## Task Kinds
//!
//! [`TaskKind`] describes what a task runs:
//!
//! | Kind         | Meaning                | Routed by runner |
//! |--------------|------------------------|------------------|
//! | `Subprocess` | Host command or script | yes              |
//! | `Container`  | OCI image              | yes              |
//! | `Wasm`       | WASI module            | yes              |
//! | `Embedded`   | In-process task        | no               |
//!
//! Routable variants are consumed by `solti-runner`.
//! Embedded tasks are submitted directly by `solti-core`.
//!
//! ## Selectors
//!
//! [`RunnerSelector`] matches runner labels. All requirements are ANDed:
//!
//! ```rust
//! use solti_model::{Labels, RunnerSelector, SelectorRequirement};
//!
//! let selector = RunnerSelector {
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
//! ## Environment
//!
//! [`TaskEnv`] comes from the task. [`RunnerEnv`] comes from the runner.
//! [`merge_env`] combines them, with runner values winning on duplicate keys.
//!
//! ## Auth
//!
//! [`Token`] is the shared bearer secret between an agent and the control plane.
//! Its `Debug` output is redacted, and [`Token::verify`] compares in constant time.
//!
//! ## Also
//!
//! - `solti-runner` consumes [`TaskSpec`] and [`TaskKind`] to build executable tasks.
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
