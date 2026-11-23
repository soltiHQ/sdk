//! Declarations of the workload types an agent can run.

use std::fmt;
use std::sync::Arc;

use serde::{Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;

use super::task_workload::{RESERVED_GROUP_REASON, in_reserved_group};
use crate::{SchemaFingerprint, Workload, WorkloadSchema, WorkloadTypeMeta};

/// Largest integer a JSON number carries exactly in an IEEE-754 double: 2^53 - 1.
///
/// Podium compiles schemas with this bound, so a 64-bit field that is not bounded by it
/// accepts values no JSON peer reads back.
pub const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// One workload type an agent declares: its GVK and the schema of its `spec`.
///
/// Two types are equal, and hash alike, when the GVK and the schema fingerprint are.
/// It serializes as the registration shape: `{"apiVersion","kind","schemaFingerprint"}`.
#[derive(Clone, Debug)]
pub struct WorkloadType {
    meta: WorkloadTypeMeta,
    schema: WorkloadSchema,
}

impl WorkloadType {
    /// Pairs a GVK with the schema of its `spec`.
    #[inline]
    pub fn new(meta: WorkloadTypeMeta, schema: WorkloadSchema) -> Self {
        Self { meta, schema }
    }

    /// GVK of the type.
    #[inline]
    pub fn meta(&self) -> &WorkloadTypeMeta {
        &self.meta
    }

    /// Workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        self.meta.api_version()
    }

    /// Workload kind.
    #[inline]
    pub fn kind(&self) -> &str {
        self.meta.kind()
    }

    /// Schema of the `spec`.
    #[inline]
    pub fn schema(&self) -> &WorkloadSchema {
        &self.schema
    }

    /// Fingerprint of the schema.
    #[inline]
    pub fn fingerprint(&self) -> &SchemaFingerprint {
        self.schema.fingerprint()
    }
}

impl PartialEq for WorkloadType {
    fn eq(&self, other: &Self) -> bool {
        self.meta == other.meta && self.fingerprint() == other.fingerprint()
    }
}

impl Eq for WorkloadType {}

impl std::hash::Hash for WorkloadType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.meta.hash(state);
        self.fingerprint().hash(state);
    }
}

impl Serialize for WorkloadType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("WorkloadType", 3)?;
        state.serialize_field("apiVersion", self.api_version())?;
        state.serialize_field("kind", self.kind())?;
        state.serialize_field("schemaFingerprint", self.fingerprint())?;
        state.end()
    }
}

/// Why a [`WorkloadDecl`] cannot become a [`WorkloadType`].
///
/// The declaration keeps it until the runner is registered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadDeclError {
    api_version: String,
    kind: String,
    reason: String,
}

impl WorkloadDeclError {
    /// The declared API group and version, as written.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// The declared kind, as written.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// What is wrong, with the location inside the schema when the schema is at fault.
    #[inline]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for WorkloadDeclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "workload declaration {}/{}: {}",
            self.api_version, self.kind, self.reason
        )
    }
}

impl std::error::Error for WorkloadDeclError {}

type Check = dyn Fn(&Workload) -> Result<(), String> + Send + Sync;

/// A workload type a runner declares: GVK, schema and the admission check of its body.
///
/// The check must be fast and non-blocking: no I/O, no locks held across a wait, no
/// heavy computation. Core calls it synchronously on every create and apply of a task of
/// this type, while it holds the task's write lock and a state-write admission permit, so a
/// slow check stalls every write to that task and takes a permit that other writes need.
/// Decode and compare; do not look anything up.
///
/// A refused GVK, a reserved group or a refused schema does not fail the constructor:
/// it is kept and reported by [`workload_type`](Self::workload_type) when the runner is registered.
#[derive(Clone)]
pub struct WorkloadDecl {
    api_version: String,
    kind: String,
    resolved: Result<WorkloadType, WorkloadDeclError>,
    check: Arc<Check>,
}

impl WorkloadDecl {
    /// Declares the type of a Rust body: its GVK, a schema derived from the type, and a check
    /// that decodes the body with it. The check's reason omits the GVK:
    /// `spec.<path>: <message>`, or `spec: <message>` at the root.
    ///
    /// The schema is draft 2020-12, with 64-bit integers bounded to the JSON-safe range.
    /// The schema carries the Rust type's name as its root `title`, the names of nested types
    /// as `$defs` keys and the doc comments as `description`s, all part of the fingerprint.
    ///
    /// The check decodes the body, so it is fast; a [`validate`](crate::WorkloadBody::validate) that does more
    /// must stay fast and non-blocking too.
    #[cfg(feature = "schema")]
    pub fn of<B: crate::WorkloadBody + schemars::JsonSchema>() -> Self {
        Self::new(
            B::API_VERSION,
            B::KIND,
            &super::generate::workload_schema_for::<B>(),
            |workload| {
                workload
                    .decode::<B>()
                    .map(|_| ())
                    .map_err(|error| error.detail())
            },
        )
    }

    /// Declares a type from a hand-written schema and an admission check.
    ///
    /// `check` runs on every routed workload of this GVK before the runner sees it, under the
    /// task's write lock and a state-write admission permit. It must be fast and non-blocking.
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        schema: &Value,
        check: impl Fn(&Workload) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        let api_version = api_version.into();
        let kind = kind.into();
        let resolved = resolve(&api_version, &kind, schema);
        Self {
            api_version,
            kind,
            resolved,
            check: Arc::new(check),
        }
    }

    /// Declares a type whose `spec` is any JSON object and whose check accepts everything.
    pub fn object(api_version: impl Into<String>, kind: impl Into<String>) -> Self {
        let schema = serde_json::json!({
            "$schema": crate::WORKLOAD_SCHEMA_DIALECT,
            "type": "object",
        });
        Self::new(api_version, kind, &schema, |_| Ok(()))
    }

    /// The declared API group and version, as written.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// The declared kind, as written.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The declared type.
    ///
    /// # Errors
    ///
    /// Returns the kept [`WorkloadDeclError`] when the GVK, its group or the schema is refused.
    pub fn workload_type(&self) -> Result<&WorkloadType, &WorkloadDeclError> {
        self.resolved.as_ref()
    }

    /// Runs the admission check on a routed workload.
    ///
    /// Core runs it under the task's write lock and a state-write admission permit, so it
    /// must be fast and non-blocking.
    ///
    /// # Errors
    ///
    /// Returns the reason the body is refused.
    pub fn check(&self, workload: &Workload) -> Result<(), String> {
        (self.check)(workload)
    }
}

impl fmt::Debug for WorkloadDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkloadDecl")
            .field("api_version", &self.api_version)
            .field("kind", &self.kind)
            .field("resolved", &self.resolved)
            .finish_non_exhaustive()
    }
}

fn resolve(
    api_version: &str,
    kind: &str,
    schema: &Value,
) -> Result<WorkloadType, WorkloadDeclError> {
    let refuse = |reason: String| WorkloadDeclError {
        api_version: api_version.to_owned(),
        kind: kind.to_owned(),
        reason,
    };
    let meta =
        WorkloadTypeMeta::new(api_version, kind).map_err(|error| refuse(error.to_string()))?;
    if in_reserved_group(api_version) {
        return Err(refuse(RESERVED_GROUP_REASON.to_owned()));
    }
    let schema = WorkloadSchema::from_value(schema).map_err(|error| refuse(error.to_string()))?;
    Ok(WorkloadType::new(meta, schema))
}

#[cfg(all(test, feature = "schema"))]
#[allow(dead_code)] // sample bodies are only deserialized and described
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    struct Sample {
        name: String,
        note: Option<String>,
        mode: Mode,
        items: Vec<Item>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    enum Mode {
        Fast,
        Slow { level: u8 },
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct Item {
        id: String,
    }

    impl crate::WorkloadBody for Sample {
        const API_VERSION: &'static str = "tasks.example.io/v1";
        const KIND: &'static str = "Sample";
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Outer {
        a: String,
        #[serde(flatten)]
        inner: Inner,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(tag = "type")]
    enum Inner {
        One { b: String },
        Two { c: u8 },
    }

    impl crate::WorkloadBody for Outer {
        const API_VERSION: &'static str = "tasks.example.io/v1";
        const KIND: &'static str = "Outer";
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct Ints {
        big: u64,
        signed: i64,
        #[schemars(range(max = 10))]
        tight: u64,
        small: u32,
        maybe: Option<u64>,
    }

    impl crate::WorkloadBody for Ints {
        const API_VERSION: &'static str = "tasks.example.io/v1";
        const KIND: &'static str = "Ints";
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct Reserved {}

    impl crate::WorkloadBody for Reserved {
        const API_VERSION: &'static str = "solti.io/v1";
        const KIND: &'static str = "Reserved";
    }

    fn schema_of(decl: &WorkloadDecl) -> Value {
        decl.workload_type().unwrap().schema().to_value()
    }

    fn contains_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(key) || map.values().any(|v| contains_key(v, key))
            }
            Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
            _ => false,
        }
    }

    fn refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    match (key.as_str(), v) {
                        ("$ref", Value::String(target)) => out.push(target.clone()),
                        _ => refs(v, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
}
