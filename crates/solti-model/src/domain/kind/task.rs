//! Typed built-in and extensible task workloads.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::{Flag, ModelError, ModelResult, SubprocessMode, TaskEnv, validation};

/// API group and version shared by built-in Solti workloads.
pub const WORKLOAD_API_VERSION: &str = "solti.io/v1";

/// API version and kind identifying one workload schema.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadTypeMeta {
    api_version: String,
    kind: String,
}

impl<'de> Deserialize<'de> for WorkloadTypeMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawWorkloadTypeMeta {
            api_version: String,
            kind: String,
        }

        let raw = RawWorkloadTypeMeta::deserialize(deserializer)?;
        Self::new(raw.api_version, raw.kind).map_err(serde::de::Error::custom)
    }
}

impl WorkloadTypeMeta {
    /// Build workload type metadata.
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

/// Desired state of an in-process task implementation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedSpec {
    revision: String,
}

impl<'de> Deserialize<'de> for EmbeddedSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawEmbeddedSpec {
            revision: String,
        }

        let raw = RawEmbeddedSpec::deserialize(deserializer)?;
        Self::new(raw.revision).map_err(serde::de::Error::custom)
    }
}

impl EmbeddedSpec {
    /// Build an embedded workload spec with an implementation revision.
    pub fn new(revision: impl Into<String>) -> ModelResult<Self> {
        let spec = Self {
            revision: revision.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Caller-controlled implementation revision.
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
/// | `Embedded`   | In-process `TaskRef`           | no       |
/// | `Wasm`       | WASI module (`.wasm`)          | yes      |
/// | `Extension`  | Application-provided runner    | yes      |
///
/// Routable variants go through `RunnerRouter::pick()`.
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

    /// Built-in / code-defined task that does not require a runner.
    ///
    /// Used only with `SupervisorApi::create_with_task()` or `SupervisorApi::apply_with_task()`.
    /// Runner-backed reconciliation must reject this workload before runner selection.
    Embedded(EmbeddedSpec),

    /// A workload implemented by an application-provided runner.
    Extension(ExtensionWorkload),
}

/// Serializable GVK envelope for an application-provided workload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionWorkload {
    api_version: String,
    kind: String,
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
    /// Build and validate an extension workload envelope.
    ///
    /// The `solti.io` API group is reserved for built-in Solti workloads.
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

    /// Opaque workload-specific desired state.
    #[inline]
    pub fn spec(&self) -> &Value {
        &self.spec
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
        Ok(())
    }
}

impl TaskWorkload {
    /// Return the workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        match self {
            Self::Extension(workload) => workload.api_version(),
            _ => WORKLOAD_API_VERSION,
        }
    }

    /// Return owned workload type metadata suitable for historical snapshots.
    pub fn type_meta(&self) -> WorkloadTypeMeta {
        WorkloadTypeMeta {
            api_version: self.api_version().to_owned(),
            kind: self.kind().to_owned(),
        }
    }

    /// Return the workload resource kind.
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

    /// Validate kind-specific constraints.
    ///
    /// Delegates to the inner spec: [`SubprocessMode::validate`], [`WasmSpec::validate`], [`ContainerSpec::validate`].
    /// `Embedded` requires a non-empty implementation revision.
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
            Self::Subprocess(spec) => spec.mode.validate(),
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
#[serde(rename_all = "camelCase")]
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
    /// Construct a WASM spec from its module path and options.
    ///
    /// `WasmSpec` is `#[non_exhaustive]`; use this constructor instead of a struct literal.
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

    /// Validate structural constraints.
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
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        if self.module.as_os_str().is_empty() {
            return Err(crate::error::ModelError::Invalid(
                "wasm module path cannot be empty".into(),
            ));
        }
        Ok(())
    }
}

impl ContainerSpec {
    /// Construct a container spec from its image and options.
    ///
    /// `ContainerSpec` is `#[non_exhaustive]`; use this constructor instead of a struct literal.
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

    /// Validate structural constraints.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv};
    ///
    /// let spec = ContainerSpec::new("redis:7".into(), None, vec![], TaskEnv::default());
    /// spec.validate().unwrap();
    /// ```
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        if self.image.trim().is_empty() {
            return Err(crate::error::ModelError::Invalid(
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
    fn task_kind_validate_rejects_empty_container_image() {
        let kind = TaskWorkload::Container(ContainerSpec {
            image: "".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        });
        let err = kind.validate().unwrap_err();
        assert!(err.to_string().contains("container image"));
    }

    #[test]
    fn task_kind_validate_rejects_whitespace_container_image() {
        let kind = TaskWorkload::Container(ContainerSpec {
            image: "  \t".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        });
        assert!(kind.validate().is_err());
    }

    #[test]
    fn task_kind_validate_rejects_empty_wasm_module() {
        let kind = TaskWorkload::Wasm(WasmSpec {
            module: PathBuf::new(),
            args: vec![],
            env: Default::default(),
        });
        let err = kind.validate().unwrap_err();
        assert!(err.to_string().contains("wasm module"));
    }

    #[test]
    fn task_kind_validate_accepts_valid_container() {
        let kind = TaskWorkload::Container(ContainerSpec {
            image: "nginx:latest".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        });
        assert!(kind.validate().is_ok());
    }

    #[test]
    fn task_kind_validate_accepts_embedded() {
        assert!(
            TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap())
                .validate()
                .is_ok()
        );
        assert!(EmbeddedSpec::new("  ").is_err());
    }

    #[test]
    fn built_in_workload_uses_gvk_envelope() {
        let json = serde_json::to_value(TaskWorkload::Embedded(
            EmbeddedSpec::new("build-42").unwrap(),
        ))
        .unwrap();

        assert_eq!(json["apiVersion"], "solti.io/v1");
        assert_eq!(json["kind"], "Embedded");
        assert_eq!(json["spec"], serde_json::json!({"revision": "build-42"}));
    }

    #[test]
    fn extension_workload_roundtrips_unknown_gvk() {
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "tasks.example.io/v1alpha1",
                "ImageResize",
                serde_json::json!({ "width": 1280, "format": "webp" }),
            )
            .unwrap(),
        );

        let json = serde_json::to_string(&workload).unwrap();
        let back: TaskWorkload = serde_json::from_str(&json).unwrap();

        assert_eq!(back, workload);
        assert_eq!(back.api_version(), "tasks.example.io/v1alpha1");
        assert_eq!(back.kind(), "ImageResize");
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
    fn extension_envelope_roundtrips_directly() {
        let workload = ExtensionWorkload::new(
            "tasks.example.io/v1",
            "Report",
            serde_json::json!({ "format": "json" }),
        )
        .unwrap();

        let json = serde_json::to_string(&workload).unwrap();
        let back: ExtensionWorkload = serde_json::from_str(&json).unwrap();

        assert_eq!(back, workload);
    }

    #[test]
    fn extension_workload_rejects_empty_gvk_parts() {
        let error = ExtensionWorkload::new("", "Example", serde_json::json!({})).unwrap_err();
        assert!(error.to_string().contains("apiVersion"));
    }

    #[test]
    fn workload_gvk_uses_kubernetes_crd_validation() {
        WorkloadTypeMeta::new("tasks.example.io/v1alpha1", "ImageResize").unwrap();
        ExtensionWorkload::new("tasks.example.io/v1", "custom-kind", serde_json::json!({}))
            .unwrap();

        for api_version in [
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
/// - script: script body passed to a [`Runtime`](crate::Runtime).
///
/// Common fields (`env`, `cwd`, `fail_on_non_zero`) apply to both modes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubprocessSpec {
    /// Execution strategy (command or script).
    pub mode: SubprocessMode,
    /// Environment variables for the process.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Whether to treat non-zero exit codes as task failure.
    ///
    /// When enabled (default), any non-zero exit code will be reported as a failure.
    #[serde(default)]
    pub fail_on_non_zero: Flag,
}

impl SubprocessSpec {
    /// Construct a subprocess spec from its execution mode and common options.
    ///
    /// `SubprocessSpec` is `#[non_exhaustive]`; use this constructor instead of a struct literal.
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
}

/// Specification for WebAssembly module execution via a WASI-compatible runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WasmSpec {
    /// Path to the `.wasm` module.
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
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ContainerSpec {
    /// Container image (e.g. `"nginx:latest"`, `"docker.io/library/redis:7"`).
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
