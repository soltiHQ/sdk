//! Task execution backend types.
//!
//! - [`TaskKind`] - enum selecting the runtime backend and its parameters.
//! - [`Runtime`] - script interpreter for subprocess script execution.
//! - [`SubprocessMode`] - execution strategy (command or script).

mod runtime;
pub use runtime::Runtime;

mod subprocess;
pub use subprocess::SubprocessMode;

mod task;
pub use task::TaskKind;
