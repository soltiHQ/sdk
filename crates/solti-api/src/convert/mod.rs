//! # Proto ↔ domain conversion.
//!
//! Two-way translation layer between [`solti_model`] domain types and generated protobuf wire types.
//! Split by target type to keep each module short and focused.
//!
//! ## Module map
//!
//! | Module  | Content                                                    |
//! |---------|------------------------------------------------------------|
//! | `spec`  | `TaskSpec` domain ↔ wire, plus all nested policy helpers   |
//! | `phase` | `TaskPhase` ↔ `TaskStatus`                                 |
//! | `time`  | `SystemTime` → Unix-ms helper                              |
//! | `meta`  | `ObjectMeta` domain → wire                                 |
//! | `run`   | `TaskRun` domain → wire                                    |
//! | `task`  | `Task` domain → wire, `TaskPage` → `ListTasksResponse`     |
//!

mod meta;
mod phase;
mod run;
mod spec;
mod task;
mod time;

#[cfg(feature = "grpc")]
pub(crate) use phase::proto_to_domain_status;
pub use spec::convert_create_spec;
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) use task::tasks_page_to_proto;
