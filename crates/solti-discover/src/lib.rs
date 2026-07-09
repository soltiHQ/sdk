//! # solti-discover - agent heartbeat.
//!
//! Periodic sync task that registers an agent with the control plane and reports liveness and platform telemetry.
//!
//! | feature | transport         | protocol                                  |
//! |---------|-------------------|-------------------------------------------|
//! | `grpc`  | tonic gRPC client | `proto/solti/discover/v1/discovery.proto` |
//! | `http`  | reqwest HTTP/JSON | `POST /api/v1/discovery/sync`             |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! # #[cfg(feature = "http")]
//! # fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! use solti_discover::{DiscoverConfig, DiscoveryTransport};
//! use solti_model::AgentId;
//!
//! let cfg = DiscoverConfig::builder(
//!     AgentId::from("agent-1"),
//!     "agent-1",               // display name
//!     "http://127.0.0.1:8085", // this agent's endpoint
//!     "http://127.0.0.1:9000", // control-plane endpoint
//!     DiscoveryTransport::Http,
//!     30_000, // sync interval (ms)
//!     1,      // api_version
//! )
//! .build()?;
//!
//! let (task, spec) = solti_discover::sync(cfg)?;
//! // Submit to a running taskvisor supervisor:
//! // supervisor.submit_with_task(task, &spec).await?;
//! # let _ = (task, spec);
//! # Ok(())
//! # }
//! ```
//!
//! ## Also
//!
//! - `DiscoverConfig` / `DiscoverConfigBuilder` (feature `grpc`/`http`): identity, endpoint, transport, timeouts, capabilities.
//! - `sync` (feature `grpc`/`http`): factory returning `Result<(TaskRef, TaskSpec), DiscoverError>`.
//! - [`DiscoverError`]: config, transport, parse, and rejection failures.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
///
/// Gated on `http` + `tls`: the README examples use the full API surface
/// (docs.rs and CI build with `--all-features`).
#[cfg(all(doctest, feature = "http", feature = "tls"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod errors;
pub use errors::DiscoverError;

mod metrics;
pub use metrics::{
    DiscoverFailReason, DiscoverMetricsBackend, DiscoverMetricsHandle, NoOpDiscoverMetrics,
    OUTCOME_FAILURE, OUTCOME_SUCCESS, noop_discover_metrics,
};

#[cfg(any(feature = "grpc", feature = "http"))]
pub use solti_model::Token;

#[cfg(any(feature = "grpc", feature = "http"))]
mod config;
#[cfg(any(feature = "grpc", feature = "http"))]
pub use config::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, DiscoverConfig, DiscoverConfigBuilder,
    DiscoveryTransport,
};

#[cfg(any(feature = "grpc", feature = "http"))]
mod tasks;
#[cfg(any(feature = "grpc", feature = "http"))]
pub use tasks::sync;

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) mod proto {
    include!(concat!(env!("OUT_DIR"), "/solti.discover.v1.rs"));

    #[cfg(feature = "http")]
    include!(concat!(env!("OUT_DIR"), "/solti.discover.v1.serde.rs"));
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub use proto::{SyncRequest, SyncResponse};
