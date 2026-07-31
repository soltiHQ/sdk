//! # solti-exec
//!
//! Execution backends for Solti workloads.
//!
//! The `subprocess` feature provides [`SubprocessRunner`](subprocess::SubprocessRunner).
//! It converts a [`solti_model::Task`] into a reusable [`taskvisor::TaskRef`].
//! Taskvisor owns execution after that conversion.
//!
//! ## Start Here
//!
//! 1. Enable the `subprocess` feature.
//! 2. Create a [`solti_runner::RunnerRouter`].
//! 3. Register a subprocess runner.
//! 4. Build a Taskvisor task from a `Subprocess` resource.
//!
//! ## Flow
//!
//! ```text
//! solti_model::Task
//!         │ Subprocess workload
//!         ▼
//!   RunnerRouter
//!         │ GVK + runnerSelector
//!         ▼
//! SubprocessRunner
//!         │ builds
//!         ▼
//! taskvisor::TaskRef
//!         │ each attempt
//!         ▼
//! operating-system process
//!    ├── stdout/stderr ──► tracing + OutputSink
//!    └── exit/cancel ────► TaskError
//! ```
//!
//! Building does not start the process.
//! Attempt-scoped files, cgroups, output sinks, and processes are created when Taskvisor runs the task.
//!
//! ## Commands and Scripts
//!
//! [`solti_model::SubprocessMode::Command`] starts an executable directly.
//! [`solti_model::SubprocessMode::Script`] uses an explicit interpreter and a base64 body.
//! The script is written to a fresh temporary file for each attempt.
//!
//! ## Backend Controls
//!
//! | Area              | Configuration                                 |
//! |-------------------|-----------------------------------------------|
//! | Environment       | [`subprocess::EnvPolicy`]                     |
//! | Working directory | [`subprocess::CwdPolicy`]                     |
//! | Output            | [`subprocess::LogConfig`]                     |
//! | POSIX limits      | [`RlimitConfig`]                              |
//! | Linux cgroup v2   | [`CgroupLimits`], [`CpuMax`]                  |
//! | Linux security    | [`SecurityConfig`], [`Namespaces`]            |
//! | Linux seccomp     | [`SeccompPolicy`] with feature `seccomp`      |
//!
//! Configure these controls through [`subprocess::SubprocessBackendConfig`].
//! Invalid or unsupported controls are rejected when the runner is created.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! # #[cfg(feature = "subprocess")]
//! # {
//! use solti_exec::subprocess::register_subprocess_runner;
//! use solti_model::{
//!     Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskSpec, TaskWorkload,
//! };
//! use solti_runner::RunnerRouter;
//!
//! let mut router = RunnerRouter::new();
//! register_subprocess_runner(&mut router, "default")?;
//!
//! let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
//!     SubprocessMode::Command {
//!         command: "echo".into(),
//!         args: vec!["hello".into()],
//!     },
//!     TaskEnv::new(),
//!     None,
//!     Flag::enabled(),
//! ));
//! let spec = TaskSpec::builder("jobs", workload, 5_000_u64).build()?;
//! let resource = Task::new("hello", spec)?;
//!
//! let task = router.build(&resource)?;
//! assert!(task.name().starts_with("default-jobs-"));
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Main Types
//!
//! | Area             | Types                                                       |
//! |------------------|-------------------------------------------------------------|
//! | Runner           | [`subprocess::SubprocessRunner`]                            |
//! | Registration     | [`subprocess::register_subprocess_runner`]                  |
//! | Backend settings | [`subprocess::SubprocessBackendConfig`]                     |
//! | Environment      | [`subprocess::EnvPolicy`], [`subprocess::CwdPolicy`]        |
//! | Output           | [`subprocess::LogConfig`]                                   |
//! | Resources        | [`RlimitConfig`], [`CgroupLimits`], [`CpuMax`]              |
//! | Security         | [`SecurityConfig`], [`Namespaces`], [`LinuxCapability`]     |
//! | Errors           | [`ExecError`]                                               |
//!
//! ## Feature Flags
//!
//! - `subprocess`: host subprocess runner.
//! - `seccomp`: Linux syscall denylist. It enables `subprocess`.
//!
//! No feature is enabled by default.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(all(doctest, feature = "subprocess"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(feature = "subprocess")]
mod error;
#[cfg(feature = "subprocess")]
#[cfg_attr(docsrs, doc(cfg(feature = "subprocess")))]
pub use error::ExecError;

#[cfg(feature = "subprocess")]
#[cfg_attr(docsrs, doc(cfg(feature = "subprocess")))]
pub use utils::{
    CgroupLimits, CpuMax, LinuxCapability, Namespaces, RlimitConfig, SeccompPolicy, SecurityConfig,
};
#[cfg(feature = "subprocess")]
#[cfg_attr(docsrs, doc(cfg(feature = "subprocess")))]
pub mod subprocess;
#[cfg(feature = "subprocess")]
pub(crate) mod utils;
