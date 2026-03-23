//! Domain primitives for the solti task model.
//!
//! ## Modules
//!
//! | Module          | Types                                                        | Purpose                                     |
//! |-----------------|--------------------------------------------------------------|---------------------------------------------|
//! | `policy/`       | [`RestartPolicy`], [`BackoffPolicy`], [`AdmissionPolicy`]    | Lifecycle and concurrency policies          |
//! | `selector/`     | [`RunnerSelector`], [`SelectorRequirement`]                  | K8s-style label selector for runner routing |
//! | `environment/`  | [`TaskEnv`], [`RunnerEnv`], [`merge_env`]                    | Env-var handling with runner-wins merge     |
//! | `query/`        | [`TaskQuery`], [`TaskPage`]                                  | Filtered, paginated task listing            |
//! | `identity/`     | [`Slot`], [`TaskId`]                                         | Resource identity (`Arc<str>` newtypes)     |
//! | `kind/`         | [`TaskKind`]                                                 | Execution backend enum                      |
//! | `label`         | [`Labels`]                                                   | Key-value metadata (`BTreeMap` newtype)     |
//! | `flag`          | [`Flag`]                                                     | Boolean toggle                              |
//! | `kv`            | [`KeyValue`]                                                 | Generic key-value pair                      |
//! | `phase`         | [`TaskPhase`]                                                | Task lifecycle state                        |
//! | `timeout`       | [`Timeout`]                                                  | Milliseconds newtype                        |

mod policy;
pub use policy::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy};

mod selector;
pub use selector::{RunnerSelector, SelectorOperator, SelectorRequirement};

mod environment;
pub use environment::{RunnerEnv, TaskEnv, merge as merge_env};

mod kind;
pub use kind::{Runtime, SubprocessMode, TaskKind};

mod query;
pub use query::{TaskPage, TaskQuery};

mod label;
pub use label::{Labels, LabelsIter};

mod identity;
pub use identity::{Slot, TaskId};

mod phase;
pub use phase::TaskPhase;

mod timeout;
pub use timeout::Timeout;

mod kv;
pub use kv::KeyValue;

mod flag;
pub use flag::Flag;
