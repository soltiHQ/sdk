//! # Native containerd 2.x engine
//!
//! [`ContainerdEngine`] implements the engine container lifecycle.
//!
//! ## Start Here
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use solti_exec::container::{
//!     containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine},
//!     register_container_runner,
//! };
//! use solti_runner::RunnerRouter;
//!
//! # async fn configure() -> Result<
//! #     (RunnerRouter, Arc<ContainerdEngine>),
//! #     Box<dyn std::error::Error>,
//! # > {
//! let settings = ContainerdConfig::new(
//!     "/run/containerd/containerd.sock",
//!     "solti",
//!     "overlayfs",
//!     "io.containerd.runc.v2",
//! )
//! .with_network(ContainerNetwork::None);
//!
//! let engine = Arc::new(ContainerdEngine::connect(settings).await?);
//! let mut router = RunnerRouter::new();
//! register_container_runner(&mut router, "containerd", engine.clone())?;
//! # Ok((router, engine))
//! # }
//! ```
//!
//! `ContainerNetwork::None` is the default.
//! It creates a network namespace without external network provisioning.
//! `ContainerNetwork::Host` shares the host network namespace.
//! The mode belongs to [`ContainerdConfig`] and therefore applies to one registered runner.
//!
//! ## Ownership
//!
//! ```text
//! connect -> cleanup runtime + blocking I/O domain
//! create  -> lifecycle admission -> shared image resolve/unpack
//!         -> I/O admission and preparation -> active attempt
//! active attempt Drop -> bounded handoff -> deferred cleanup
//! explicit cleanup    -> remote release -> local I/O release
//! ```
//!
//! Lifecycle admission is reserved before image resolution. It stays charged
//! through the returned attempt or deferred cleanup. The default limit is 1024
//! admitted create or attempt lifecycles per engine. Shared image resolution
//! and unpack are charged to that admission, but their transfer is not tracked
//! by attempt cleanup or handed to the cleanup domain when the create future is
//! dropped. After attempt ownership begins, cancellation transfers confirmed or
//! uncertain ownership without waiting.
//! Cleanup and blocking-I/O queues allocate nodes only for admitted operations.
//! They do not reserve queue storage proportional to the configured limit. The
//! admission semaphores remain exact bounds over active and queued ownership.
//! Identity mismatch prevents adoption and deletion.
//! The configured containerd namespace must not replace an attempt resource
//! between identity verification and deletion.
//!
//! Keep the concrete engine handle returned by `configure`.
//! [`ContainerdEngine::shutdown`] closes lifecycle admission and waits for every
//! accepted create lifecycle and attempt owner. Call it after all supervisors
//! that use the engine stop.
//!
//! ## Best effort
//!
//! Deferred cleanup covers future cancellation and Tokio runtime shutdown in
//! the current process. It does not survive process abort, power loss, or
//! `SIGKILL`.

mod image;
mod io;
mod io_domain;
mod rpc;
mod spec;

mod cleanup;
mod config;
pub use config::{ContainerNetwork, ContainerPlatform, ContainerdConfig};

mod engine;
pub use engine::ContainerdEngine;
