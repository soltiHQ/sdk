//! # Domain types
//!
//! ## Modules
//!
//! | Module          | Types                                                             | Purpose                                     |
//! |-----------------|-------------------------------------------------------------------|---------------------------------------------|
//! | `policy/`       | [`RestartPolicy`], [`BackoffPolicy`], [`AdmissionPolicy`]         | Lifecycle and concurrency policies          |
//! | `selector/`     | [`LabelSelector`], [`SelectorRequirement`]                        | Label selector for routing                  |
//! | `environment/`  | [`TaskEnv`]                                                       | Task-provided environment variables         |
//! | `query/`        | [`TaskQuery`], [`TaskRunQuery`], continuations and pages          | Task filtering and collections              |
//! | `identity/`     | [`AgentId`], [`Slot`], [`TaskId`]                                 | Resource identity (`Arc<str>`)              |
//! | `kind/`         | [`TaskWorkload`]                                                  | Typed and extensible workload model         |
//! | `label`         | [`Labels`]                                                        | Key-value metadata (`BTreeMap`)             |
//! | `flag`          | [`Flag`]                                                          | Boolean toggle                              |
//! | `kv`            | [`KeyValue`]                                                      | Generic key-value pair                      |
//! | `phase`         | [`TaskPhase`]                                                     | Task lifecycle state                        |
//! | `timeout`       | [`Timeout`]                                                       | Milliseconds                                |
//! | `capability`    | [`AgentCapabilities`], [`RunnerCapability`]                       | Agent execution capabilities                |

mod capability;
pub use capability::{AgentCapabilities, RunnerCapability};

mod kind;
pub use kind::{
    ContainerSpec, EmbeddedSpec, ExtensionWorkload, MAX_SCRIPT_BODY_BYTES, SubprocessMode,
    SubprocessSpec, TaskWorkload, WORKLOAD_API_VERSION, WasmSpec, WorkloadTypeMeta,
};

mod identity;
pub use identity::{AGENT_ID_MAX_LEN, AgentId, SLOT_MAX_LEN, Slot, TASK_ID_MAX_LEN, TaskId};

mod policy;
pub use policy::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy};

mod selector;
pub use selector::{LabelSelector, SelectorOperator, SelectorRequirement};

mod environment;
pub use environment::TaskEnv;

mod query;
pub use query::{
    DEFAULT_LIMIT, DEFAULT_TASK_RUN_LIMIT, MAX_LIMIT, MAX_TASK_PAGE_ITEM_BYTES, MAX_TASK_RUN_LIMIT,
    MAX_TASK_RUN_PAGE_ITEM_BYTES, TaskContinuation, TaskFilter, TaskPage, TaskQuery,
    TaskRunContinuation, TaskRunPage, TaskRunQuery, TaskWatchEvent,
};

mod label;
pub use label::{Labels, LabelsIter};

mod phase;
pub use phase::TaskPhase;

mod timeout;
pub use timeout::Timeout;

mod kv;
pub use kv::KeyValue;

mod flag;
pub use flag::Flag;

mod output;
pub use output::{OutputChunk, OutputEvent, StreamKind};
