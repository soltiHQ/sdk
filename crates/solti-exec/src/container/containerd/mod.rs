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
//! # async fn configure() -> Result<(), Box<dyn std::error::Error>> {
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
//! register_container_runner(&mut router, "containerd", engine)?;
//! # Ok(())
//! # }
//! ```
//!
//! `ContainerNetwork::None` is the default.
//! It creates a network namespace without external network provisioning.
//! `ContainerNetwork::Host` shares the host network namespace.
//! The mode belongs to [`ContainerdConfig`] and therefore applies to one registered runner.

mod image;
mod io;
mod spec;

mod config;
pub use config::{ContainerNetwork, ContainerPlatform, ContainerdConfig};

mod engine;
pub use engine::ContainerdEngine;
