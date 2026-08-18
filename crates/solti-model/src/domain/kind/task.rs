//! # Task workloads
//!
//! [`TaskWorkload`] contains built-in and extension workload desired state.
//!
//! Every workload uses a Kubernetes-style envelope:
//!
//! ```text
//! apiVersion
//! kind
//! spec
//! ```
//!
//! Built-in workload specs reject unknown fields.
//! Extension specs preserve application-owned JSON object fields.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{Flag, ModelError, ModelResult, SubprocessMode, TaskEnv, validation};

/// API group and version of built-in Solti workloads.
pub const WORKLOAD_API_VERSION: &str = "solti.io/v1";

const MAX_EXTENSION_JSON_DEPTH: usize = 128;

/// Group/version and kind of one workload schema.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
pub struct WorkloadTypeMeta {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::crd_api_version")
    )]
    api_version: String,
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::crd_kind"))]
    kind: String,
}

impl<'de> Deserialize<'de> for WorkloadTypeMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct RawWorkloadTypeMeta {
            api_version: String,
            kind: String,
        }

        let raw = RawWorkloadTypeMeta::deserialize(deserializer)?;
        Self::new(raw.api_version, raw.kind).map_err(serde::de::Error::custom)
    }
}

impl WorkloadTypeMeta {
    /// Creates validated workload type metadata.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for an invalid CRD group/version or kind.
    pub fn new(api_version: impl Into<String>, kind: impl Into<String>) -> ModelResult<Self> {
        let type_meta = Self {
            api_version: api_version.into(),
            kind: kind.into(),
        };
        type_meta.validate()?;
        Ok(type_meta)
    }

    /// Workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Workload resource kind.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    fn validate(&self) -> ModelResult<()> {
        validation::validate_crd_api_version("workload apiVersion", &self.api_version)?;
        validation::validate_crd_kind("workload kind", &self.kind)
    }
}

/// Desired state of an embedded task implementation.
///
/// The revision participates in desired-state comparison.
/// The runtime task handle is supplied separately by a higher layer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedSpec {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::non_empty_string")
    )]
    revision: String,
}

impl<'de> Deserialize<'de> for EmbeddedSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawEmbeddedSpec {
            revision: String,
        }

        let raw = RawEmbeddedSpec::deserialize(deserializer)?;
        Self::new(raw.revision).map_err(serde::de::Error::custom)
    }
}

impl EmbeddedSpec {
    /// Creates an embedded workload spec.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `revision` is empty.
    pub fn new(revision: impl Into<String>) -> ModelResult<Self> {
        let spec = Self {
            revision: revision.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Caller-owned implementation revision.
    #[inline]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    fn validate(&self) -> ModelResult<()> {
        if self.revision.trim().is_empty() {
            return Err(ModelError::Invalid(
                "embedded workload revision must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// Executable desired state nested in a [`TaskSpec`](crate::TaskSpec).
///
/// | Variant      | Backend                        | Routable |
/// |--------------|--------------------------------|----------|
/// | `Subprocess` | OS process (`command`, `args`) | yes      |
/// | `Container`  | OCI container image            | yes      |
/// | `Embedded`   | In-process implementation      | no       |
/// | `Wasm`       | WASI module (`.wasm`)          | yes      |
/// | `Extension`  | Application-provided runner    | yes      |
///
/// Routable variants are selected by `solti-runner`.
/// `Embedded` carries no runtime task handle.
///
/// ## Example
///
/// ```
/// use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskWorkload};
///
/// let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
///     SubprocessMode::Command {
///         command: "echo".into(),
///         args: vec!["hello".into()],
///     },
///     TaskEnv::default(),
///     None,
///     Flag::enabled(),
/// ));
///
/// assert_eq!(workload.kind(), "Subprocess");
/// workload.validate().unwrap();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskWorkload {
    /// Execute a subprocess on the host.
    Subprocess(SubprocessSpec),

    /// Execute a WebAssembly module via a WASI-compatible runtime.
    Wasm(WasmSpec),

    /// Run a task inside an OCI-compatible container.
    Container(ContainerSpec),

    /// Code-defined task that bypasses runner routing.
    ///
    /// A higher layer binds the desired revision to an in-process task.
    Embedded(EmbeddedSpec),

    /// Workload implemented by an application-provided runner.
    Extension(ExtensionWorkload),
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for TaskWorkload {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TaskWorkload".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let subprocess =
            workload_envelope_schema("Subprocess", generator.subschema_for::<SubprocessSpec>());
        let wasm = workload_envelope_schema("Wasm", generator.subschema_for::<WasmSpec>());
        let container =
            workload_envelope_schema("Container", generator.subschema_for::<ContainerSpec>());
        let embedded =
            workload_envelope_schema("Embedded", generator.subschema_for::<EmbeddedSpec>());
        let extension = generator.subschema_for::<ExtensionWorkload>();

        schemars::json_schema!({
            "description": "Kubernetes-style workload GVK and desired state.",
            "oneOf": [subprocess, wasm, container, embedded, extension]
        })
    }
}

#[cfg(feature = "schema")]
fn workload_envelope_schema(kind: &'static str, spec: schemars::Schema) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["apiVersion", "kind", "spec"],
        "properties": {
            "apiVersion": {
                "type": "string",
                "const": WORKLOAD_API_VERSION
            },
            "kind": {
                "type": "string",
                "const": kind
            },
            "spec": spec
        }
    })
}

/// GVK envelope for an application-provided workload.
///
/// `spec` must be a JSON object.
/// Its fields are owned by the application.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
pub struct ExtensionWorkload {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::extension_api_version")
    )]
    api_version: String,
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::crd_kind"))]
    kind: String,
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::json_object")
    )]
    spec: Value,
}

impl<'de> Deserialize<'de> for ExtensionWorkload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWorkloadEnvelope::deserialize(deserializer)?;
        Self::new(raw.api_version, raw.kind, raw.spec).map_err(serde::de::Error::custom)
    }
}

impl ExtensionWorkload {
    /// Creates an extension workload envelope.
    ///
    /// The `solti.io` API group is reserved for built-in Solti workloads.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for an invalid GVK, a reserved API group, or a non-object `spec`.
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        spec: Value,
    ) -> ModelResult<Self> {
        let workload = Self {
            api_version: api_version.into(),
            kind: kind.into(),
            spec,
        };
        workload.validate()?;
        Ok(workload)
    }

    /// Workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Workload resource kind.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Application-owned desired state.
    #[inline]
    pub fn spec(&self) -> &Value {
        &self.spec
    }

    /// Splits the extension envelope into its GVK and application-owned spec.
    pub fn into_parts(self) -> (String, String, Value) {
        (self.api_version, self.kind, self.spec)
    }

    fn validate(&self) -> ModelResult<()> {
        let group = validation::validate_crd_api_version(
            "extension workload apiVersion",
            &self.api_version,
        )?;
        validation::validate_crd_kind("extension workload kind", &self.kind)?;
        if group == "solti.io" {
            return Err(ModelError::Invalid(
                format!(
                    "extension workload GVK {}/{} uses the reserved solti.io API group",
                    self.api_version, self.kind
                )
                .into(),
            ));
        }
        if !self.spec.is_object() {
            return Err(ModelError::Invalid(
                "extension workload spec must be a JSON object".into(),
            ));
        }
        validate_extension_depth(&self.spec)?;
        Ok(())
    }
}

fn validate_extension_depth(root: &Value) -> ModelResult<()> {
    let mut pending = vec![(root, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_EXTENSION_JSON_DEPTH {
            return Err(ModelError::Invalid(
                format!("extension workload spec depth exceeds max {MAX_EXTENSION_JSON_DEPTH}")
                    .into(),
            ));
        }
        match value {
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    Ok(())
}

impl TaskWorkload {
    /// Returns the workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        match self {
            Self::Extension(workload) => workload.api_version(),
            _ => WORKLOAD_API_VERSION,
        }
    }

    /// Returns owned workload type metadata.
    pub fn type_meta(&self) -> WorkloadTypeMeta {
        WorkloadTypeMeta {
            api_version: self.api_version().to_owned(),
            kind: self.kind().to_owned(),
        }
    }

    /// Returns the workload resource kind.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskWorkload;
    ///
    /// let embedded = TaskWorkload::Embedded(solti_model::EmbeddedSpec::new("v1").unwrap());
    /// assert_eq!(embedded.kind(), "Embedded");
    /// ```
    #[inline]
    pub fn kind(&self) -> &str {
        match self {
            Self::Subprocess(_) => "Subprocess",
            Self::Container(_) => "Container",
            Self::Embedded(_) => "Embedded",
            Self::Wasm(_) => "Wasm",
            Self::Extension(workload) => workload.kind(),
        }
    }

    /// Validates kind-specific constraints.
    ///
    /// Delegates to the inner workload spec.
    /// `Embedded` requires a non-empty implementation revision.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the selected spec is invalid.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv, TaskWorkload};
    ///
    /// let workload = TaskWorkload::Container(ContainerSpec::new(
    ///     "redis:7".into(),
    ///     None,
    ///     vec![],
    ///     TaskEnv::default(),
    /// ));
    ///
    /// workload.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        match self {
            Self::Subprocess(spec) => spec.validate(),
            Self::Container(spec) => spec.validate(),
            Self::Wasm(spec) => spec.validate(),
            Self::Embedded(spec) => spec.validate(),
            Self::Extension(workload) => workload.validate(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkloadEnvelope<'a, T> {
    api_version: &'a str,
    kind: &'a str,
    spec: T,
}

impl Serialize for TaskWorkload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Subprocess(spec) => WorkloadEnvelope {
                api_version: self.api_version(),
                kind: self.kind(),
                spec,
            }
            .serialize(serializer),
            Self::Wasm(spec) => WorkloadEnvelope {
                api_version: self.api_version(),
                kind: self.kind(),
                spec,
            }
            .serialize(serializer),
            Self::Container(spec) => WorkloadEnvelope {
                api_version: self.api_version(),
                kind: self.kind(),
                spec,
            }
            .serialize(serializer),
            Self::Embedded(spec) => WorkloadEnvelope {
                api_version: self.api_version(),
                kind: self.kind(),
                spec,
            }
            .serialize(serializer),
            Self::Extension(workload) => WorkloadEnvelope {
                api_version: workload.api_version(),
                kind: workload.kind(),
                spec: workload.spec(),
            }
            .serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawWorkloadEnvelope {
    api_version: String,
    kind: String,
    spec: Value,
}

impl<'de> Deserialize<'de> for TaskWorkload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWorkloadEnvelope::deserialize(deserializer)?;
        let workload = if raw.api_version == WORKLOAD_API_VERSION {
            match raw.kind.as_str() {
                "Subprocess" => Self::Subprocess(
                    serde_json::from_value(raw.spec).map_err(serde::de::Error::custom)?,
                ),
                "Wasm" => {
                    Self::Wasm(serde_json::from_value(raw.spec).map_err(serde::de::Error::custom)?)
                }
                "Container" => Self::Container(
                    serde_json::from_value(raw.spec).map_err(serde::de::Error::custom)?,
                ),
                "Embedded" => Self::Embedded(
                    serde_json::from_value(raw.spec).map_err(serde::de::Error::custom)?,
                ),
                _ => Self::Extension(
                    ExtensionWorkload::new(raw.api_version, raw.kind, raw.spec)
                        .map_err(serde::de::Error::custom)?,
                ),
            }
        } else {
            Self::Extension(
                ExtensionWorkload::new(raw.api_version, raw.kind, raw.spec)
                    .map_err(serde::de::Error::custom)?,
            )
        };
        workload.validate().map_err(serde::de::Error::custom)?;
        Ok(workload)
    }
}

impl WasmSpec {
    /// Creates a WASM spec.
    ///
    /// `WasmSpec` is `#[non_exhaustive]`.
    /// Use this constructor outside the crate.
    /// Validation occurs when the workload enters a [`crate::TaskSpec`].
    ///
    /// ## Example
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use solti_model::{TaskEnv, WasmSpec};
    ///
    /// let spec = WasmSpec::new(PathBuf::from("job.wasm"), vec!["--help".into()], TaskEnv::default());
    /// assert_eq!(spec.module, PathBuf::from("job.wasm"));
    /// ```
    pub fn new(module: PathBuf, args: Vec<String>, env: TaskEnv) -> Self {
        Self { module, args, env }
    }

    /// Validates structural constraints.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the module path is empty or cannot
    /// be represented as UTF-8 on the HTTP and gRPC wire.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use solti_model::{TaskEnv, WasmSpec};
    ///
    /// let spec = WasmSpec::new(PathBuf::from("job.wasm"), vec![], TaskEnv::default());
    /// spec.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        if self.module.as_os_str().is_empty() {
            return Err(ModelError::Invalid(
                "wasm module path cannot be empty".into(),
            ));
        }
        validate_wire_path("wasm module path", &self.module)?;
        Ok(())
    }
}

impl ContainerSpec {
    /// Creates a container spec.
    ///
    /// `ContainerSpec` is `#[non_exhaustive]`.
    /// Use this constructor outside the crate.
    /// Validation occurs when the workload enters a [`crate::TaskSpec`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv};
    ///
    /// let spec = ContainerSpec::new(
    ///     "docker.io/library/redis:7".into(),
    ///     None,
    ///     vec![],
    ///     TaskEnv::default(),
    /// );
    ///
    /// assert_eq!(spec.image, "docker.io/library/redis:7");
    /// ```
    pub fn new(
        image: String,
        command: Option<Vec<String>>,
        args: Vec<String>,
        env: TaskEnv,
    ) -> Self {
        Self {
            image,
            command,
            args,
            env,
        }
    }

    /// Validates structural constraints.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the image is empty.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv};
    ///
    /// let spec = ContainerSpec::new("redis:7".into(), None, vec![], TaskEnv::default());
    /// spec.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        if self.image.trim().is_empty() {
            return Err(ModelError::Invalid(
                "container image cannot be empty".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn built_in_validation_accepts_valid_values_and_rejects_empty_fields() {
        TaskWorkload::Container(ContainerSpec {
            image: "nginx:latest".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        })
        .validate()
        .unwrap();
        TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap())
            .validate()
            .unwrap();
        // `cwd` remains optional text; UTF-8 validity does not make it non-empty.
        TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: Vec::new(),
            },
            TaskEnv::default(),
            Some(PathBuf::new()),
            Flag::enabled(),
        ))
        .validate()
        .unwrap();

        for image in ["", "  \t"] {
            let workload = TaskWorkload::Container(ContainerSpec {
                image: image.into(),
                command: None,
                args: vec![],
                env: Default::default(),
            });
            let error = workload.validate().unwrap_err();
            assert!(error.to_string().contains("container image"));
        }

        let workload = TaskWorkload::Wasm(WasmSpec {
            module: PathBuf::new(),
            args: vec![],
            env: Default::default(),
        });
        let error = workload.validate().unwrap_err();
        assert!(error.to_string().contains("wasm module"));
        assert!(EmbeddedSpec::new("  ").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_task_specs_reject_non_utf8_wire_paths_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let invalid_path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        let subprocess = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: Vec::new(),
            },
            TaskEnv::default(),
            Some(invalid_path.clone()),
            Flag::enabled(),
        ));
        let error = crate::TaskSpec::builder("native", subprocess.clone(), 1_000u64)
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("subprocess cwd"));
        assert!(error.to_string().contains("UTF-8"));
        assert!(serde_json::to_value(subprocess).is_err());

        let wasm = TaskWorkload::Wasm(WasmSpec::new(invalid_path, Vec::new(), TaskEnv::default()));
        let error = crate::TaskSpec::builder("native", wasm.clone(), 1_000u64)
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("wasm module path"));
        assert!(error.to_string().contains("UTF-8"));
        assert!(serde_json::to_value(wasm).is_err());
    }

    #[test]
    fn built_in_envelope_has_stable_gvk_and_rejects_unknown_fields() {
        let workload = TaskWorkload::Embedded(EmbeddedSpec::new("build-42").unwrap());
        let json = serde_json::to_value(&workload).unwrap();

        assert_eq!(json["apiVersion"], "solti.io/v1");
        assert_eq!(json["kind"], "Embedded");
        assert_eq!(json["spec"], serde_json::json!({"revision": "build-42"}));

        let mut envelope = serde_json::to_value(&workload).unwrap();
        envelope["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskWorkload>(envelope).is_err());

        let mut spec = serde_json::to_value(workload).unwrap();
        spec["spec"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskWorkload>(spec).is_err());
    }

    #[test]
    fn extension_roundtrips_gvk_and_application_owned_fields() {
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "tasks.example.io/v1alpha1",
                "ImageResize",
                serde_json::json!({
                    "width": 1280,
                    "format": "webp",
                    "unexpectedToSolti": true,
                    "nested": { "applicationField": [1, 2, 3] }
                }),
            )
            .unwrap(),
        );

        let json = serde_json::to_string(&workload).unwrap();
        let back: TaskWorkload = serde_json::from_str(&json).unwrap();

        assert_eq!(back, workload);
        assert_eq!(back.api_version(), "tasks.example.io/v1alpha1");
        assert_eq!(back.kind(), "ImageResize");

        let extension = ExtensionWorkload::new(
            "tasks.example.io/v1",
            "Report",
            serde_json::json!({ "format": "json" }),
        )
        .unwrap();
        let json = serde_json::to_string(&extension).unwrap();
        assert_eq!(
            serde_json::from_str::<ExtensionWorkload>(&json).unwrap(),
            extension
        );
    }

    #[test]
    fn extension_workload_rejects_reserved_solti_api_group() {
        for api_version in [WORKLOAD_API_VERSION, "solti.io/v2"] {
            let error =
                ExtensionWorkload::new(api_version, "Custom", serde_json::json!({})).unwrap_err();

            assert!(
                error.to_string().contains("reserved"),
                "apiVersion={api_version}"
            );
        }
    }

    #[test]
    fn extension_workload_allows_builtin_kind_in_another_api_version() {
        for kind in ["Subprocess", "Wasm", "Container", "Embedded"] {
            let workload = TaskWorkload::Extension(
                ExtensionWorkload::new(
                    "tasks.example.io/v1",
                    kind,
                    serde_json::json!({ "custom": true }),
                )
                .unwrap(),
            );

            let json = serde_json::to_string(&workload).unwrap();
            let back: TaskWorkload = serde_json::from_str(&json).unwrap();
            assert_eq!(back, workload, "kind={kind}");
        }
    }

    #[test]
    fn workload_gvk_uses_kubernetes_crd_validation() {
        WorkloadTypeMeta::new("tasks.example.io/v1alpha1", "ImageResize").unwrap();
        ExtensionWorkload::new("tasks.example.io/v1", "custom-kind", serde_json::json!({}))
            .unwrap();

        for api_version in [
            "",
            " solti.io/v1",
            "bad/version/extra",
            "example/v1",
            "tasks.example.io/1v",
        ] {
            assert!(
                ExtensionWorkload::new(api_version, "Example", serde_json::json!({})).is_err(),
                "apiVersion={api_version}"
            );
        }
        for kind in ["", "1Example", "Bad Kind", "_Example"] {
            assert!(
                ExtensionWorkload::new("tasks.example.io/v1", kind, serde_json::json!({})).is_err(),
                "kind={kind}"
            );
        }
    }

    #[test]
    fn extension_workload_requires_object_spec() {
        let error =
            ExtensionWorkload::new("example.io/v1", "Example", serde_json::json!(42)).unwrap_err();

        assert!(error.to_string().contains("JSON object"));
    }

    #[test]
    fn extension_workload_rejects_excessive_json_depth() {
        let mut value = serde_json::json!(true);
        for _ in 0..=MAX_EXTENSION_JSON_DEPTH {
            value = serde_json::json!({ "next": value });
        }

        let error = ExtensionWorkload::new("example.io/v1", "Example", value).unwrap_err();
        assert!(error.to_string().contains("depth exceeds"));
    }

    #[test]
    fn constructors_build_specs_with_expected_fields() {
        use crate::{Flag, SubprocessMode, TaskEnv};

        let sub = SubprocessSpec::new(
            SubprocessMode::Command {
                command: "ls".into(),
                args: vec!["-l".into()],
            },
            TaskEnv::default(),
            Some(PathBuf::from("/tmp")),
            Flag::enabled(),
        );
        assert!(matches!(sub.mode, SubprocessMode::Command { .. }));
        assert_eq!(sub.cwd, Some(PathBuf::from("/tmp")));

        let wasm = WasmSpec::new(
            PathBuf::from("/m.wasm"),
            vec!["--x".into()],
            TaskEnv::default(),
        );
        assert_eq!(wasm.module, PathBuf::from("/m.wasm"));
        assert_eq!(wasm.args, vec!["--x".to_string()]);

        let cont = ContainerSpec::new(
            "img:1".into(),
            Some(vec!["sh".into()]),
            vec!["-c".into()],
            TaskEnv::default(),
        );
        assert_eq!(cont.image, "img:1");
        assert_eq!(cont.command, Some(vec!["sh".to_string()]));
    }
}

/// Specification for subprocess execution on the host.
///
/// Supports two execution strategies via [`SubprocessMode`]:
/// - command: direct binary execution;
/// - script: script body passed to an explicit interpreter.
///
/// Common fields (`env`, `cwd`, `fail_on_non_zero`) apply to both modes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct SubprocessSpec {
    /// Execution strategy (command or script).
    pub mode: SubprocessMode,
    /// Environment variables for the process.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
    /// Working directory.
    ///
    /// A validated task spec requires this wire-facing path to be valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Whether to treat non-zero exit codes as task failure.
    ///
    /// When enabled (default), any non-zero exit code will be reported as a failure.
    #[serde(default)]
    pub fail_on_non_zero: Flag,
}

impl SubprocessSpec {
    /// Creates a subprocess spec.
    ///
    /// `SubprocessSpec` is `#[non_exhaustive]`.
    /// Use this constructor outside the crate.
    /// Validation occurs when the workload enters a [`crate::TaskSpec`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv};
    ///
    /// let spec = SubprocessSpec::new(
    ///     SubprocessMode::Command {
    ///         command: "echo".into(),
    ///         args: vec!["hello".into()],
    ///     },
    ///     TaskEnv::default(),
    ///     None,
    ///     Flag::enabled(),
    /// );
    ///
    /// assert!(spec.fail_on_non_zero.is_enabled());
    /// ```
    pub fn new(
        mode: SubprocessMode,
        env: TaskEnv,
        cwd: Option<PathBuf>,
        fail_on_non_zero: Flag,
    ) -> Self {
        Self {
            mode,
            env,
            cwd,
            fail_on_non_zero,
        }
    }

    /// Validates the subprocess mode and wire-facing path fields.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the mode is invalid or `cwd`
    /// cannot be represented as UTF-8 on the HTTP and gRPC wire.
    pub fn validate(&self) -> ModelResult<()> {
        self.mode.validate()?;
        if let Some(cwd) = &self.cwd {
            validate_wire_path("subprocess cwd", cwd)?;
        }
        Ok(())
    }
}

fn validate_wire_path(field: &str, path: &Path) -> ModelResult<()> {
    if path.to_str().is_none() {
        return Err(ModelError::Invalid(
            format!("{field} must contain valid UTF-8").into(),
        ));
    }
    Ok(())
}

/// Specification for WebAssembly module execution via a WASI-compatible runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct WasmSpec {
    /// Path to the `.wasm` module.
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::path_string")
    )]
    pub module: PathBuf,
    /// Arguments passed to the WASI main entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables exposed to the WASI module.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
}

/// Specification for OCI-compatible container execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// Container image (e.g. `"nginx:latest"`, `"docker.io/library/redis:7"`).
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::non_empty_string")
    )]
    pub image: String,
    /// Override container entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Arguments passed to the container entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the container.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
}
