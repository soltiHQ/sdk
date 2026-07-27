//! # solti-exec
//!
//! Runs Solti subprocess workloads as operating-system processes.
//! The runner supports output streaming, cancellation, resource limits, and
//! Linux process security.
//!
//! Enable the `subprocess` feature to use this crate.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! # #[cfg(feature = "subprocess")]
//! # {
//! use solti_exec::subprocess::{SubprocessRunner, register_subprocess_runner};
//! use solti_model::{SubprocessMode, SubprocessSpec, Task, TaskSpec, TaskWorkload};
//! use solti_runner::{BuildContext, Runner, RunnerRouter, make_run_id};
//!
//! let mut router = RunnerRouter::new();
//! register_subprocess_runner(&mut router, "default")?;
//!
//! // A runner can also be used directly.
//! let runner = SubprocessRunner::new("direct")?;
//! let spec = TaskSpec::builder(
//!     "hello",
//!     TaskWorkload::Subprocess(SubprocessSpec::new(
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
//! let resource = Task::new("hello", spec)?;
//! let run_id = make_run_id(runner.name(), resource.slot().as_str());
//! let task = runner.build_task(&resource, &run_id, &BuildContext::default())?;
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`solti_runner::RunnerRouter`] selects a runner from workload GVK and labels.
//! [`solti_runner::Runner`] is the backend contract.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(all(doctest, feature = "subprocess"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(feature = "subprocess")]
mod error;
#[cfg(feature = "subprocess")]
pub use error::ExecError;

#[cfg(feature = "subprocess")]
pub use utils::{
    CgroupLimits, CpuMax, LinuxCapability, Namespaces, RlimitConfig, SeccompPolicy, SecurityConfig,
};
#[cfg(feature = "subprocess")]
pub mod subprocess;
#[cfg(feature = "subprocess")]
pub(crate) mod utils;
