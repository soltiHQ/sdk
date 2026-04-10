//! # solti-exec - task execution backends.
//!
//! Provides concrete [`Runner`](solti_runner::Runner) implementations that turn [`TaskSpec`](solti_model::TaskSpec)
//! into running OS processes (and, in the future, WASM / container backends).
//!
//! ## Feature flags
//!
//! | Flag          | What it enables                                             |
//! |---------------|-------------------------------------------------------------|
//! | `subprocess`  | [`subprocess`] module — OS process runner with sandboxing   |
//!
//! ## Quick start
//!
//! ```text
//! use solti_exec::subprocess::*;
//!
//! let mut router = RunnerRouter::new();
//! register_subprocess_runner(&mut router, "default")?;
//!
//! // with sandboxing
//! let backend = SubprocessBackendConfig::new()
//!     .with_rlimits(RlimitConfig { .. })
//!     .with_cgroups(CgroupLimits { .. })
//!     .with_security(SecurityConfig { .. });
//! register_subprocess_runner_with_backend(&mut router, "secure", backend)?;
//! ```
//!
//! ## Also
//!
//! - [`solti_runner::Runner`] trait implemented by backends in this crate.
//! - [`solti_model::TaskKind`] determines which backend handles the task.
//! - [`solti_runner::RunnerRouter`] routes tasks to registered runners.

mod error;
pub use error::ExecError;

mod metrics;

#[cfg(feature = "subprocess")]
pub use utils::{CgroupLimits, CpuMax, LinuxCapability, RlimitConfig, SecurityConfig};
#[cfg(feature = "subprocess")]
pub mod subprocess;
#[cfg(feature = "subprocess")]
pub(crate) mod utils;
