//! # Protobuf/domain conversion.
//!
//! Two-way translation layer between [`solti_model`] domain types and generated protobuf wire types.
//! Split by target type to keep each module short and focused.
//!
//! ## Module map
//!
//! | Module  | Content                                                    |
//! |---------|------------------------------------------------------------|
//! | `spec`  | `TaskSpec` domain ↔ wire, plus all nested policy helpers   |
//! | `phase` | `TaskPhase` domain enum ↔ wire enum                        |
//! | `time`  | `SystemTime` → Unix-ms helper                              |
//! | `meta`  | `ObjectMeta` domain → wire                                 |
//! | `run`   | `TaskRun` domain → wire                                    |
//! | `task`  | `Task` domain → wire, `TaskPage` → `ListTasksResponse`     |
//!

mod condition;
mod meta;
mod phase;
mod preconditions;
mod run;
mod spec;
mod task;
mod time;

mod output;
mod policy;
mod selector;
mod workload;
pub(crate) use output::output_event_to_proto;
pub(crate) use phase::proto_to_domain_phase;
pub(crate) use preconditions::write_preconditions_from_proto;
pub(crate) use task::task_manifest_from_proto;
pub(crate) use task::task_watch_event_to_proto;
pub(crate) use task::tasks_page_to_proto;
