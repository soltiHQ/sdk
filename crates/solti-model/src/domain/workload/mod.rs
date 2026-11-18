//! # Workload types
//!
//! [`WorkloadSchema`] is the JSON Schema a runner publishes for one workload
//! type: RFC 8785 canonical bytes, their [`SchemaFingerprint`], and the check
//! that keeps the schema inside the draft 2020-12 subset Podium compiles.

mod schema;
pub use schema::{
    MAX_SCHEMA_REGEX_INSTRUCTIONS, MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL,
    MAX_SCHEMA_REGEX_SOURCE_BYTES, MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL,
    MAX_SCHEMA_REGEX_UNICODE_ESCAPES, MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL,
    MAX_WORKLOAD_SCHEMA_BYTES, MAX_WORKLOAD_SCHEMA_DEPTH, MAX_WORKLOAD_SCHEMA_VALUES,
    SchemaFingerprint, SdkStrictRegexRule, WORKLOAD_SCHEMA_DIALECT, WorkloadSchema,
    WorkloadSchemaError, WorkloadSchemaErrorReason, canonical_json,
};
