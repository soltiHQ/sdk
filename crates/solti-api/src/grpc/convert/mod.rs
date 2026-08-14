//! # Protobuf Conversion
//!
//! Translation between [`solti_model`] and generated gRPC messages.
//!
//! ```text
//! protobuf request ── validate ──► domain value ── ApiHandler
//! protobuf response ◄──────────── domain value ◄─────┘
//! ```
//!
//! Request conversion rejects invalid or unspecified wire values.
//! Response conversion rejects domain variants without a v1 wire representation.
//!
//! ## Modules
//!
//! | Module          | Direction      | Values                              |
//! |-----------------|----------------|-------------------------------------|
//! | `task`          | Both           | Manifest, resource, page, watch     |
//! | `spec`          | Both           | Task spec                           |
//! | `workload`      | Both           | Built-in and extension workloads    |
//! | `policy`        | Both           | Restart, backoff, admission         |
//! | `selector`      | Both           | Labels and runner selector          |
//! | `phase`         | Both           | Task phase                          |
//! | `preconditions` | Request        | Conditional writes                  |
//! | `condition`     | Response       | Reconciliation condition            |
//! | `meta`          | Response       | Object metadata                     |
//! | `run`           | Response       | Run history                         |
//! | `output`        | Response       | Live output                         |
//! | `time`          | Response       | Unix millisecond timestamps         |

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
pub(crate) use run::runs_page_to_proto_bounded;
pub(crate) use task::task_manifest_from_proto;
pub(crate) use task::task_watch_event_to_proto;
pub(crate) use task::tasks_page_to_proto_bounded;
