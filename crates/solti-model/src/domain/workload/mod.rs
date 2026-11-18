//! # Workload types
//!
//! [`WorkloadSchema`] is the JSON Schema a runner publishes for one workload
//! type: RFC 8785 canonical bytes, their [`SchemaFingerprint`], and the check
//! that keeps the schema inside the draft 2020-12 subset Podium compiles.
//!
//! [`TaskWorkload`] is the desired state a task runs: a [`Workload`] routed by
//! its GVK, or the SDK-internal [`EmbeddedSpec`]. [`WorkloadBody`] decodes a
//! routed body into a typed Rust value.

mod gvk;
pub use gvk::WorkloadTypeMeta;

mod body;
pub use body::{WorkloadBody, WorkloadBodyError, WorkloadBodyErrorKind};

mod task_workload;
pub use task_workload::{
    EMBEDDED_WORKLOAD_API_VERSION, EMBEDDED_WORKLOAD_KIND, EmbeddedSpec, MAX_WORKLOAD_BODY_DEPTH,
    TaskWorkload, Workload,
};
pub(crate) use task_workload::{RESERVED_GROUP_REASON, in_reserved_group};

mod decl;
pub use decl::{MAX_JSON_SAFE_INTEGER, WorkloadDecl, WorkloadDeclError, WorkloadType};

#[cfg(feature = "schema")]
mod generate;
#[cfg(feature = "schema")]
pub use generate::{BoundJsonSafeIntegers, workload_schema_for};

mod schema;
pub use schema::{
    MAX_SCHEMA_REGEX_INSTRUCTIONS, MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
    MAX_SCHEMA_REGEX_SOURCE_BYTES, MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
    MAX_SCHEMA_REGEX_UNICODE_ESCAPES, MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL,
    MAX_WORKLOAD_SCHEMA_BYTES, MAX_WORKLOAD_SCHEMA_DEPTH, MAX_WORKLOAD_SCHEMA_VALUES,
    SchemaFingerprint, SdkStrictRegexRule, WORKLOAD_SCHEMA_DIALECT, WorkloadSchema,
    WorkloadSchemaError, WorkloadSchemaErrorReason, canonical_json,
};
