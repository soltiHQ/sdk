//! # solti
//!
//! Feature and namespace façade for the modular Solti SDK.
//! It contains no runtime logic.
//! Features select component crates and expose them under stable namespaces.
//!
//! Use this crate when an agent binary needs several SDK components.
//! Depend on a component crate directly when one component is enough.
//!
//! ## Start Here
//!
//! 1. Choose the capabilities required by the binary.
//! 2. Enable their `solti` features.
//! 3. Import each component through its namespace.
//! 4. Build and own the application runtime.
//!
//! All features are disabled by default.
//! Higher-level features enable their required lower layers.
//!
//! ## Feature Flow
//!
//! ```text
//! application features
//!          ▼
//!        solti
//!          ├── model ─────────────► solti-model
//!          ├── model-schema ──────► solti-model + JSON Schema
//!          ├── runner ────────────► solti-runner + model + taskvisor
//!          ├── chain ─────────────► solti-chain + runner
//!          ├── core ──────────────► solti-core + runner + taskvisor/controller
//!          ├── exec-* ────────────► solti-exec integrations
//!          ├── api-* ─────────────► solti-api transports and adapters
//!          ├── discover-* ────────► solti-discover transports
//!          ├── observe-* ─────────► solti-observe integrations
//!          ├── prometheus-* ──────► solti-prometheus integrations
//!          └── tls ───────────────► solti-tls
//! ```
//!
//! Component crates never depend on this façade.
//! The façade preserves their ownership boundaries.
//!
//! ## Choose Features
//!
//! | Need                         | Feature family          |
//! |------------------------------|-------------------------|
//! | Resource types               | `model`                 |
//! | Resource JSON Schema         | `model-schema`          |
//! | Runner registration          | `runner`                |
//! | Conditional workload chain   | `chain`                 |
//! | Chain JSON Schema            | `chain-schema`          |
//! | Desired-state supervision    | `core`                  |
//! | Host process controls        | `exec-host-process`     |
//! | Subprocess execution         | `exec-subprocess`       |
//! | Container engine boundary    | `exec-container`        |
//! | Native containerd 2.x        | `exec-containerd`       |
//! | Seccomp host process filter  | `exec-seccomp`          |
//! | HTTP task API                | `api-http`              |
//! | gRPC task API                | `api-grpc`              |
//! | Core API adapter             | `api-core-adapter`      |
//! | gRPC server TLS              | `api-grpc-tls`          |
//! | HTTP discovery               | `discover-http`         |
//! | gRPC discovery               | `discover-grpc`         |
//! | HTTP discovery with TLS      | `discover-http-tls`     |
//! | gRPC discovery with TLS      | `discover-grpc-tls`     |
//! | TLS extension for discovery  | `discover-tls`          |
//! | Logging integrations         | `observe-*`             |
//! | Prometheus integrations      | `prometheus-*`          |
//! | Shared TLS types             | `tls`                   |
//! | Taskvisor integrations       | `taskvisor-*`           |
//!
//! `exec-seccomp` enables the low-level host process filter.
//! Combine it with `exec-subprocess` to filter subprocess attempts.
//! `exec-container` exposes the engine-neutral container runner.
//! `exec-containerd` adds the native containerd 2.x adapter.
//!
//! `api-http` and `api-grpc` expose transports.
//! They do not enable `solti-core`.
//! Add `api-core-adapter` when the API delegates to `core::SupervisorApi`.
//!
//! `discover-http` and `discover-grpc` select the outbound discovery transport.
//! Add `discover-http-tls` or `discover-grpc-tls` when that transport needs custom TLS support.
//! `discover-tls` remains available as a transport-neutral extension feature.
//! Discovery features do not select the task API exposed by the agent.
//!
//! `model` exposes runtime model types without JSON Schema dependencies.
//! Add `model-schema` when the application generates model schemas.
//! `chain-schema` enables schemas for both Chain and the nested model types.
//! `chain` runs nested workloads sequentially inside one outer Task.
//!
//! Exactly one step is active at a time; steps do not receive independent Task lifecycle policies, status, or history.
//! The `full` feature enables the complete standard integration set.
//!
//! ## Namespaces
//!
//! | Namespace      | Component crate       |
//! |----------------|-----------------------|
//! | `api`          | `solti-api`           |
//! | `chain`        | `solti-chain`         |
//! | `core`         | `solti-core`          |
//! | `discover`     | `solti-discover`      |
//! | `exec`         | `solti-exec`          |
//! | `model`        | `solti-model`         |
//! | `observe`      | `solti-observe`       |
//! | `prometheus`   | `solti-prometheus`    |
//! | `runner`       | `solti-runner`        |
//! | `taskvisor`    | `taskvisor`           |
//! | `tls`          | `solti-tls`           |
//!
//! A namespace exists only when its owning feature is enabled.
//! Re-exported APIs keep their component-crate paths below that namespace.
//!
//! ## Quick Start
//!
//! Enable only the components used by the binary:
//!
//! ```toml
//! [dependencies]
//! solti = { version = "0.0", features = [
//!     "api-core-adapter",
//!     "api-http",
//!     "exec-subprocess",
//! ] }
//! ```
//!
//! Use the canonical namespaces:
//!
//! ```rust,no_run
//! # #[cfg(all(feature = "core", feature = "exec-subprocess"))]
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//!
//! use solti::core::SupervisorApi;
//! use solti::exec::subprocess::register_subprocess_runner;
//! use solti::runner::RunnerRouter;
//!
//! let mut router = RunnerRouter::new();
//! let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
//!
//! let supervisor = SupervisorApi::builder(router).start().await?;
//! // Run application work, then stop task supervision before subprocess cleanup.
//! supervisor.shutdown().await?;
//! subprocess_runner.shutdown(Duration::from_secs(5)).await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles the runnable Rust code blocks in the facade README as doctests.
#[cfg(all(doctest, feature = "full"))]
#[doc = include_str!("../README.md")]
struct FacadeReadmeDoctests;

/// Task API types from `solti-api`.
#[cfg(feature = "api")]
#[cfg_attr(docsrs, doc(cfg(feature = "api")))]
pub use solti_api as api;

/// Conditional composite workloads from `solti-chain`.
#[cfg(feature = "chain")]
#[cfg_attr(docsrs, doc(cfg(feature = "chain")))]
pub use solti_chain as chain;

/// Desired-state supervisor types from `solti-core`.
#[cfg(feature = "core")]
#[cfg_attr(docsrs, doc(cfg(feature = "core")))]
pub use solti_core as core;

/// Agent discovery types from `solti-discover`.
#[cfg(feature = "discover")]
#[cfg_attr(docsrs, doc(cfg(feature = "discover")))]
pub use solti_discover as discover;

/// Execution integrations from `solti-exec`.
#[cfg(feature = "exec")]
#[cfg_attr(docsrs, doc(cfg(feature = "exec")))]
pub use solti_exec as exec;

/// Resource and domain types from `solti-model`.
#[cfg(feature = "model")]
#[cfg_attr(docsrs, doc(cfg(feature = "model")))]
pub use solti_model as model;

/// Observability integrations from `solti-observe`.
#[cfg(feature = "observe")]
#[cfg_attr(docsrs, doc(cfg(feature = "observe")))]
pub use solti_observe as observe;

/// Prometheus integrations from `solti-prometheus`.
#[cfg(feature = "prometheus-base")]
#[cfg_attr(docsrs, doc(cfg(feature = "prometheus-base")))]
pub use solti_prometheus as prometheus;

/// Runner contracts and routing from `solti-runner`.
#[cfg(feature = "runner")]
#[cfg_attr(docsrs, doc(cfg(feature = "runner")))]
pub use solti_runner as runner;

/// Task supervision types from `taskvisor`.
#[cfg(feature = "taskvisor")]
#[cfg_attr(docsrs, doc(cfg(feature = "taskvisor")))]
pub use taskvisor;

/// TLS and mTLS configuration types from `solti-tls`.
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use solti_tls as tls;
