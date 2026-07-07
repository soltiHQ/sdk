//! # solti-exec - task execution backends.
//!
//! Provides concrete [`Runner`](solti_runner::Runner) implementations that turn [`TaskSpec`](solti_model::TaskSpec)
//! into running OS processes (and, in the future, WASM / container backends).
//!
//! ## Feature flags
//!
//! | Flag          | What it enables                                             |
//! |---------------|-------------------------------------------------------------|
//! | `subprocess`  | `subprocess` module - OS process runner with sandboxing     |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! # #[cfg(feature = "subprocess")]
//! # {
//! use solti_exec::subprocess::{SubprocessRunner, register_subprocess_runner};
//! use solti_model::{SubprocessMode, SubprocessSpec, TaskKind, TaskSpec};
//! use solti_runner::{BuildContext, Runner, RunnerRouter};
//!
//! // The usual path: register the runner in a router.
//! let mut router = RunnerRouter::new();
//! register_subprocess_runner(&mut router, "default")?;
//!
//! // Or drive the runner directly: TaskSpec -> runnable task.
//! let runner = SubprocessRunner::new("direct");
//! let spec = TaskSpec::builder(
//!     "hello",
//!     TaskKind::Subprocess(SubprocessSpec::new(
//!         SubprocessMode::Command {
//!             command: "echo".into(),
//!             args: vec!["hi".into()],
//!         },
//!         Default::default(),
//!         None,
//!         Default::default(),
//!     )),
//!     5_000u64,
//! )
//! .build()?;
//! let task = runner.build_task(&spec, &BuildContext::default())?;
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Also
//!
//! - [`solti_runner::Runner`] trait implemented by backends in this crate.
//! - [`solti_model::TaskKind`] determines which backend handles the task.
//! - [`solti_runner::RunnerRouter`] routes tasks to registered runners.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(all(doctest, feature = "subprocess"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::ExecError;

#[cfg(feature = "subprocess")]
mod metrics;

#[cfg(feature = "subprocess")]
pub use utils::{
    CgroupLimits, CpuMax, LinuxCapability, Namespaces, RlimitConfig, SeccompPolicy, SecurityConfig,
};
#[cfg(feature = "subprocess")]
pub mod subprocess;
#[cfg(feature = "subprocess")]
pub(crate) mod utils;
