//! # Task execution backends.
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
/// ## Also
///
/// - [`TaskSpec`](crate::TaskSpec) — embeds `TaskKind` as its execution backend.
/// - `solti_runner::RunnerRouter` — picks a runner based on kind and selector.
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
    /// Returns a short symbolic identifier for the runtime kind.
    ///
    /// This is primarily intended for logging, metrics and routing:
    /// - `"subprocess"`
    /// - `"container"`
    /// - `"embedded"`
    /// - `"wasm"`
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
    /// Every variant's spec is checked here so the model is self-consistent
    /// without relying on transport-layer validators. If a new kind is
    /// added, extend this match — a missing branch is a compile error
    /// thanks to exhaustive matching.
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        match self {
            TaskKind::Subprocess(spec) => spec.mode.validate(),
            TaskKind::Wasm(spec) => spec.validate(),
            TaskKind::Container(spec) => spec.validate(),
            TaskKind::Embedded => Ok(()),
        }
    }
}

impl WasmSpec {
    /// Validate structural constraints.
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
    /// Validate structural constraints.
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
}

/// Specification for subprocess execution on the host.
///
/// Supports two execution strategies via [`SubprocessMode`]:
/// - **Command** — direct binary execution (`execve(command, args)`)
/// - **Script** — script interpreted by a [`Runtime`](crate::Runtime) (`execve(runtime, [flag, body, ...args])`)
///
/// Common fields (`env`, `cwd`, `fail_on_non_zero`) apply to both modes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// Specification for WebAssembly module execution via a WASI-compatible runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
