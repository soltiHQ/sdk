//! Task execution backends.
//!
//! [`TaskKind`] defines what a task actually runs: subprocess, WASM, container, or embedded code.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Flag, SubprocessMode, TaskEnv};

/// Execution backend for a task.
///
/// | Variant      | Backend                        | Routable |
/// |--------------|--------------------------------|----------|
/// | `Subprocess` | OS process (`command`, `args`) | yes      |
/// | `Container`  | OCI container image            | yes      |
/// | `Embedded`   | In-process `TaskRef`           | no       |
/// | `Wasm`       | WASI module (`.wasm`)          | yes      |
///
/// Routable variants go through `RunnerRouter::pick()`.
///
/// ## Example
///
/// ```
/// use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind};
///
/// let kind = TaskKind::Subprocess(SubprocessSpec::new(
///     SubprocessMode::Command {
///         command: "echo".into(),
///         args: vec!["hello".into()],
///     },
///     TaskEnv::default(),
///     None,
///     Flag::enabled(),
/// ));
///
/// assert_eq!(kind.kind(), "subprocess");
/// kind.validate().unwrap();
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TaskKind {
    /// Execute a subprocess on the host.
    Subprocess(SubprocessSpec),

    /// Execute a WebAssembly module via a WASI-compatible runtime.
    Wasm(WasmSpec),

    /// Run a task inside an OCI-compatible container.
    Container(ContainerSpec),

    /// Built-in / code-defined task that does not require a runner.
    ///
    /// Used only with `SupervisorApi::submit_with_task()`.
    /// Any attempt to submit this via `submit()` (which builds via runners) must be rejected.
    Embedded,
}

impl TaskKind {
    /// Return a short symbolic identifier for the runtime kind.
    ///
    /// This is primarily intended for logging, metrics and routing:
    /// - `"subprocess"`
    /// - `"container"`
    /// - `"embedded"`
    /// - `"wasm"`
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskKind;
    ///
    /// assert_eq!(TaskKind::Embedded.kind(), "embedded");
    /// ```
    #[inline]
    pub fn kind(&self) -> &'static str {
        match self {
            TaskKind::Subprocess(_) => "subprocess",
            TaskKind::Container(_) => "container",
            TaskKind::Embedded => "embedded",
            TaskKind::Wasm(_) => "wasm",
        }
    }

    /// Validate kind-specific constraints.
    ///
    /// Delegates to the inner spec: [`SubprocessMode::validate`], [`WasmSpec::validate`], [`ContainerSpec::validate`].
    /// `Embedded` always passes.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv, TaskKind};
    ///
    /// let kind = TaskKind::Container(ContainerSpec::new(
    ///     "redis:7".into(),
    ///     None,
    ///     vec![],
    ///     TaskEnv::default(),
    /// ));
    ///
    /// kind.validate().unwrap();
    /// ```
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        match self {
            TaskKind::Subprocess(spec) => spec.mode.validate(),
            TaskKind::Container(spec) => spec.validate(),
            TaskKind::Wasm(spec) => spec.validate(),
            TaskKind::Embedded => Ok(()),
        }
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
        let kind = TaskKind::Container(ContainerSpec {
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
        let kind = TaskKind::Container(ContainerSpec {
            image: "  \t".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        });
        assert!(kind.validate().is_err());
    }

    #[test]
    fn task_kind_validate_rejects_empty_wasm_module() {
        let kind = TaskKind::Wasm(WasmSpec {
            module: PathBuf::new(),
            args: vec![],
            env: Default::default(),
        });
        let err = kind.validate().unwrap_err();
        assert!(err.to_string().contains("wasm module"));
    }

    #[test]
    fn task_kind_validate_accepts_valid_container() {
        let kind = TaskKind::Container(ContainerSpec {
            image: "nginx:latest".into(),
            command: None,
            args: vec![],
            env: Default::default(),
        });
        assert!(kind.validate().is_ok());
    }

    #[test]
    fn task_kind_validate_accepts_embedded() {
        assert!(TaskKind::Embedded.validate().is_ok());
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
