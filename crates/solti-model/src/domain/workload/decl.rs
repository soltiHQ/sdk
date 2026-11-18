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
    /// that decodes the body with it.
    ///
    /// The schema is draft 2020-12, with 64-bit integers bounded to the JSON-safe range.
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
                    .map_err(|error| error.to_string())
            },
        )
    }

    /// Declares a type from a hand-written schema and an admission check.
    ///
    /// `check` runs on every routed workload of this GVK before the runner sees it.
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
    use serde_json::json;

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
    struct Lookahead {
        #[schemars(regex(pattern = "^(?=a)b$"))]
        text: String,
    }

    impl crate::WorkloadBody for Lookahead {
        const API_VERSION: &'static str = "tasks.example.io/v1";
        const KIND: &'static str = "Lookahead";
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

    #[test]
    fn decl_of_derives_a_2020_12_schema_inside_the_subset() {
        let decl = WorkloadDecl::of::<Sample>();
        let schema = schema_of(&decl);

        assert_eq!(schema["$schema"], crate::WORKLOAD_SCHEMA_DIALECT);
        assert!(schema["$defs"].is_object());
        let mut found = Vec::new();
        refs(&schema, &mut found);
        assert!(!found.is_empty());
        assert!(found.iter().all(|target| target.starts_with("#/$defs/")));
        assert_eq!(decl.api_version(), "tasks.example.io/v1");
        assert_eq!(decl.kind(), "Sample");
    }

    #[test]
    fn decl_of_replaces_unevaluated_properties() {
        let decl = WorkloadDecl::of::<Outer>();
        assert!(decl.workload_type().is_ok(), "{:?}", decl.workload_type());
        assert!(!contains_key(&schema_of(&decl), "unevaluatedProperties"));
    }

    #[test]
    fn decl_of_bounds_64_bit_integers_to_the_json_safe_range() {
        let schema = schema_of(&WorkloadDecl::of::<Ints>());
        let props = &schema["properties"];

        assert_eq!(props["big"]["maximum"], 9_007_199_254_740_991_i64);
        assert_eq!(props["big"]["minimum"], 0);
        assert_eq!(props["signed"]["maximum"], 9_007_199_254_740_991_i64);
        assert_eq!(props["signed"]["minimum"], -9_007_199_254_740_991_i64);
        assert_eq!(props["tight"]["maximum"], 10);
        assert_eq!(props["tight"]["minimum"], 0);
        assert!(props["small"].get("maximum").is_none());
        assert_eq!(props["small"]["minimum"], 0);
        assert_eq!(props["maybe"]["maximum"], 9_007_199_254_740_991_i64);
        assert_eq!(props["maybe"]["minimum"], 0);
    }

    #[test]
    fn decl_keeps_errors_for_registration() {
        let lookahead = WorkloadDecl::of::<Lookahead>();
        let error = lookahead.workload_type().unwrap_err();
        assert_eq!(error.api_version(), "tasks.example.io/v1");
        assert_eq!(error.kind(), "Lookahead");
        assert!(error.reason().contains("#/properties/text"), "{error}");

        let reserved = WorkloadDecl::of::<Reserved>();
        let error = reserved.workload_type().unwrap_err();
        assert!(error.reason().contains("reserved"), "{error}");
        assert!(
            error
                .reason()
                .contains("API group solti.io is reserved for SDK-internal workloads")
        );

        let bad_gvk = WorkloadDecl::object("not a gvk", "Kind");
        assert!(bad_gvk.workload_type().is_err());
        assert_eq!(bad_gvk.api_version(), "not a gvk");
    }

    #[test]
    fn decl_check_decodes_with_the_body_type() {
        let decl = WorkloadDecl::of::<Sample>();
        let good = Workload::new(
            "tasks.example.io/v1",
            "Sample",
            json!({"name": "n", "mode": "Fast", "items": []}),
        )
        .unwrap();
        assert!(decl.check(&good).is_ok());

        let bad = Workload::new(
            "tasks.example.io/v1",
            "Sample",
            json!({"name": 1, "mode": "Fast", "items": []}),
        )
        .unwrap();
        let reason = decl.check(&bad).unwrap_err();
        assert!(reason.contains("name"), "{reason}");
    }

    #[test]
    fn decl_object_accepts_any_object() {
        let decl = WorkloadDecl::object("tasks.example.io/v1", "Free");
        assert!(decl.workload_type().is_ok());
        let workload =
            Workload::new("tasks.example.io/v1", "Free", json!({"anything": [1, 2]})).unwrap();
        assert!(decl.check(&workload).is_ok());
    }

    #[test]
    fn workload_type_serializes_the_fingerprint_not_the_schema() {
        let decl = WorkloadDecl::object("tasks.example.io/v1", "Free");
        let ty = decl.workload_type().unwrap();
        assert_eq!(
            serde_json::to_value(ty).unwrap(),
            json!({
                "apiVersion": "tasks.example.io/v1",
                "kind": "Free",
                "schemaFingerprint": ty.fingerprint().as_str(),
            })
        );
    }
}
