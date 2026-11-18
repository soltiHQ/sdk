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
//! | `label`         | [`Labels`]                                                        | Key-value metadata (`BTreeMap`)             |
//! | `flag`          | [`Flag`]                                                          | Boolean toggle                              |
//! | `kv`            | [`KeyValue`]                                                      | Generic key-value pair                      |
//! | `phase`         | [`TaskPhase`]                                                     | Task lifecycle state                        |
//! | `timeout`       | [`Timeout`]                                                       | Milliseconds                                |
//! | `capability`    | [`AgentCapabilities`], [`RunnerCapability`]                       | Agent execution capabilities                |
//! | `workload/`     | [`TaskWorkload`], [`Workload`], [`WorkloadBody`], [`WorkloadSchema`] | Routed workloads and their schemas       |

mod capability;
pub use capability::{
    AgentCapabilities, MAX_AGENT_RUNNERS, MAX_AGENT_WORKLOAD_SCHEMAS, MAX_RUNNER_WORKLOAD_TYPES,
    RunnerCapability,
};

mod workload;
#[cfg(feature = "schema")]
pub use workload::{BoundJsonSafeIntegers, workload_schema_for};
pub use workload::{
    EMBEDDED_WORKLOAD_API_VERSION, EMBEDDED_WORKLOAD_KIND, EmbeddedSpec, MAX_JSON_SAFE_INTEGER,
    MAX_SCHEMA_REGEX_INSTRUCTIONS, MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
    MAX_SCHEMA_REGEX_SOURCE_BYTES, MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
    MAX_SCHEMA_REGEX_UNICODE_ESCAPES, MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL,
    MAX_WORKLOAD_BODY_DEPTH, MAX_WORKLOAD_SCHEMA_BYTES, MAX_WORKLOAD_SCHEMA_DEPTH,
    MAX_WORKLOAD_SCHEMA_VALUES, SchemaFingerprint, SdkStrictRegexRule, TaskWorkload,
    WORKLOAD_SCHEMA_DIALECT, Workload, WorkloadBody, WorkloadBodyError, WorkloadBodyErrorKind,
    WorkloadDecl, WorkloadDeclError, WorkloadSchema, WorkloadSchemaError,
    WorkloadSchemaErrorReason, WorkloadType, WorkloadTypeMeta, canonical_json,
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
