//! # Task workloads
//!
//! [`TaskWorkload`] is the workload envelope.
//! Built-in specs describe subprocess, container, WASM, and embedded execution.
//! [`ExtensionWorkload`] carries an application-owned GVK and JSON spec.
mod task;
pub use task::{
    ContainerSpec, EmbeddedSpec, ExtensionWorkload, SubprocessSpec, TaskWorkload,
    WORKLOAD_API_VERSION, WasmSpec, WorkloadTypeMeta,
};

mod subprocess;
pub use subprocess::{MAX_SCRIPT_BODY_BYTES, SubprocessMode};
