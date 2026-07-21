//! Task execution backend types.
//!
//! - [`SubprocessSpec`], [`ContainerSpec`], [`WasmSpec`] - per-variant configuration.
//! - [`TaskWorkload`] - typed built-ins plus an open extension envelope.
//! - [`Runtime`] - script interpreter for subprocess script execution.
//! - [`SubprocessMode`] - execution strategy (command or script).
mod task;
pub use task::{
    ContainerSpec, EmbeddedSpec, ExtensionWorkload, SubprocessSpec, TaskWorkload,
    WORKLOAD_API_VERSION, WasmSpec, WorkloadTypeMeta,
};

mod runtime;
pub use runtime::Runtime;

mod subprocess;
pub use subprocess::{MAX_SCRIPT_BODY_BYTES, SubprocessMode};
