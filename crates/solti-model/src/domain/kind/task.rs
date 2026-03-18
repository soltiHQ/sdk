use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Flag, TaskEnv};

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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TaskKind {
    /// Execute a native process on the host.
    Subprocess {
        /// Command to execute (e.g., `"ls"`, `"/usr/bin/python"`).
        command: String,
        /// Command-line arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Environment variables for the process.
        #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
        env: TaskEnv,
        /// Working directory.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        /// Whether to treat non-zero exit codes as task failure.
        ///
        /// When enabled (default), any non-zero exit code will be reported as a failure.
        #[serde(default)]
        fail_on_non_zero: Flag,
    },

    /// Execute a WebAssembly module via a WASI-compatible runtime.
    Wasm {
        /// Path to the `.wasm` module.
        module: PathBuf,
        /// Arguments passed to the WASI main entrypoint.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Environment variables exposed to the WASI module.
        #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
        env: TaskEnv,
    },

    /// Run a task inside an OCI-compatible container.
    Container {
        /// Container image (e.g. `"nginx:latest"`, `"docker.io/library/redis:7"`).
        image: String,
        /// Override container entrypoint.
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
        /// Arguments passed to the container entrypoint.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Environment variables for the container.
        #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
        env: TaskEnv,
    },

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
            TaskKind::Subprocess { .. } => "subprocess",
            TaskKind::Container { .. } => "container",
            TaskKind::Embedded => "embedded",
            TaskKind::Wasm { .. } => "wasm",
        }
    }
}
