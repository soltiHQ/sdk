//! # solti-exec
//!
//! Execution backends and host process controls for Solti workloads.
//!
//! The `host-process` feature provides policy and low-level process controls.
//! The `subprocess` feature provides `SubprocessRunner`.
//! The `container` feature provides an engine-neutral `ContainerRunner`.
//! The `containerd` feature provides its native containerd 2.x engine.
//! Both runners convert `solti_model::Task` resources into reusable `taskvisor::TaskRef` values.
//! `RunnerRouter` pairs each task with its allocated run identity in a
//! `solti_runner::BuiltTask`. Taskvisor owns execution after conversion.
//!
//! ## Start Here
//!
//! 1. Enable one execution backend.
//! 2. Create a `solti_runner::RunnerRouter`.
//! 3. Register its runner.
//! 4. Build a Taskvisor task from the matching workload resource.
//!
//! ## Flow
//!
//! ```text
//!                    solti_model::Task
//!                            │ GVK + runnerSelector
//!                            ▼
//!                      RunnerRouter
//!                       ├──────────┐
//!          Subprocess workload    Container workload
//!                       ▼          ▼
//!              SubprocessRunner  ContainerRunner
//!                       │          │ ContainerEngine
//!                       ▼          ▼
//!              operating-system  native containerd 2.x
//!                    process      container task
//!                       └────┬─────┘
//!                            ▼
//!          OutputSink + optional tracing copy
//!                            ▼
//!                 terminate, wait, cleanup
//! ```
//!
//! Building performs no process or engine I/O.
//! An explicit subprocess working directory is resolved and descriptor-pinned
//! on the runner-owned bounded cwd worker during build.
//! Runtime resources and output streams are attempt-scoped.
//!
//! ## Commands and Scripts
//!
//! `solti_model::SubprocessMode::Command` starts an executable directly.
//! `solti_model::SubprocessMode::Script` uses an explicit interpreter and a base64 body.
//! Each attempt transports the script through fresh backing storage.
//! Unix uses an anonymous descriptor.
//! Linux seals that descriptor before process creation.
//!
//! ## Backend Controls
//!
//! | Area              | Configuration                                      |
//! |-------------------|----------------------------------------------------|
//! | Environment       | `subprocess::EnvPolicy`                            |
//! | Working directory | `subprocess::CwdPolicy`                            |
//! | Output            | `subprocess::LogConfig`                            |
//! | Host process      | `host::HostProcessPolicy`                          |
//! | Process state     | `host::ProcessConfig`                              |
//! | POSIX limits      | `host::RlimitConfig`                               |
//! | Linux cgroup v2   | `host::CgroupLimits`, `host::CpuMax`               |
//! | Linux credentials | `host::ProcessCredentials`                         |
//! | Linux security    | `host::SecurityConfig`, `host::Namespaces`         |
//! | Linux seccomp     | `host::SeccompPolicy` with feature `seccomp`       |
//! | Container process | `container::ContainerProcessPolicy`                |
//! | Container engine  | `container::ContainerEngine`                       |
//! | Native containerd | `container::containerd::ContainerdConfig`          |
//!
//! Build host controls through `host::HostProcessPolicy`.
//! Pass that policy to `subprocess::SubprocessBackendConfig`.
//! Invalid or unsupported controls are rejected when the runner is created.
//! Applying a prepared attempt returns `host::AttemptProcessDomain`.
//! A custom backend owns its process-specific termination boundary.
//! It handles the cgroup termination result, waits and reaps, then cleans up the domain.
//!
//! An empty policy creates no cgroup.
//! A configured attempt sets `cgroup.max.depth` to zero before process creation.
//! A configured cgroup uses `cgroup.kill` when the running kernel provides it.
//! Unix subprocess attempts own a session and process group.
//! They signal the process group and a running leader before reap.
//! Without `cgroup.kill`, only the process-group boundary remains.
//! That boundary cannot reach descendants that enter another process group or session.
//!
//! ## Subprocess Quick Start
//!
//! ```rust,no_run
//! # #[cfg(feature = "subprocess")]
//! # {
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use solti_exec::subprocess::register_subprocess_runner;
//! use solti_model::{
//!     Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskSpec, TaskWorkload,
//! };
//! use solti_runner::RunnerRouter;
//! use std::time::Duration;
//!
//! let mut router = RunnerRouter::new();
//! let runner = register_subprocess_runner(&mut router, "default")?;
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
//! let built = router.build(&resource).await?;
//! assert!(built.name().starts_with("default-jobs-"));
//! drop(built);
//! drop(router);
//! runner.shutdown(Duration::from_secs(5)).await?;
//! # Ok(())
//! # }
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Main Types
//!
//! | Area                | Types                                                       |
//! |---------------------|-------------------------------------------------------------|
//! | Host process policy | `host::HostProcessPolicy`, `host::AttemptProcessDomain`     |
//! | Host process state  | `host::ProcessConfig`                                       |
//! | Host resources      | `host::RlimitConfig`, `host::CgroupLimits`                  |
//! | Host security       | `host::SecurityConfig`, `host::ProcessCredentials`          |
//! | Runner              | `subprocess::SubprocessRunner`                              |
//! | Registration        | `subprocess::register_subprocess_runner`                    |
//! | Backend settings    | `subprocess::SubprocessBackendConfig`                       |
//! | Environment         | `subprocess::EnvPolicy`, `subprocess::CwdPolicy`            |
//! | Output              | `subprocess::LogConfig`                                     |
//! | Container runner    | `container::ContainerRunner`                                |
//! | Container boundary  | `container::ContainerEngine`, `container::ContainerAttempt` |
//! | Containerd 2.x      | `container::containerd::ContainerdEngine`                   |
//! | Errors              | `ExecError`                                                 |
//!
//! ## Feature Flags
//!
//! - `host-process`: host process policy and low-level controls.
//! - `subprocess`: subprocess runner. It enables `host-process`.
//! - `seccomp`: Linux syscall denylist. It enables `host-process`.
//! - `container`: engine-neutral container runner.
//! - `containerd`: native containerd 2.x engine. It enables `container`.
//!
//! Enable both `subprocess` and `seccomp` to filter subprocess attempts.
//! Containerd execution requires Linux and an explicit containerd 2.x Unix socket.
//! The adapter does not use CRI or configure container networking.
//!
//! No feature is enabled by default.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(all(doctest, feature = "subprocess"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[cfg(any(feature = "subprocess", feature = "container"))]
mod error;
#[cfg(any(feature = "subprocess", feature = "container"))]
pub use error::ExecError;

#[cfg(any(feature = "subprocess", feature = "container"))]
mod output;
#[cfg(any(feature = "subprocess", feature = "container"))]
mod registration;

#[cfg(feature = "container")]
#[cfg_attr(docsrs, doc(cfg(feature = "container")))]
pub mod container;

#[cfg(any(feature = "container", feature = "host-process"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "container", feature = "host-process"))))]
pub mod isolation;

#[cfg(feature = "host-process")]
#[cfg_attr(docsrs, doc(cfg(feature = "host-process")))]
pub mod host;
#[cfg(feature = "subprocess")]
#[cfg_attr(docsrs, doc(cfg(feature = "subprocess")))]
pub mod subprocess;
