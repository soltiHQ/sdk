//! Task execution backend types.
//!
//! - [`SubprocessSpec`], [`ContainerSpec`], [`WasmSpec`] - per-variant configuration.
//! - [`TaskKind`] - enum selecting the runtime backend and its parameters.
//! - [`Runtime`] - script interpreter for subprocess script execution.
//! - [`SubprocessMode`] - execution strategy (command or script).
mod task;
pub use task::{ContainerSpec, SubprocessSpec, TaskKind, WasmSpec};

mod runtime;
pub use runtime::Runtime;

mod subprocess;
pub use subprocess::SubprocessMode;
