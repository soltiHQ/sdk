//! # solti-discover - agent heartbeat.
//!
//! Periodic sync task that registers an agent with the control plane and reports liveness and platform telemetry.
//!
//! | feature | transport         | protocol                      |
//! |---------|-------------------|-------------------------------|
//! | `grpc`  | tonic gRPC client | `proto/v1/sync.proto`         |
//! | `http`  | reqwest HTTP/JSON | `POST /api/v1/discovery/sync` |
//!
//! ## Quick start
//!
//! ```text
//! let cfg = DiscoverConfig::builder(/* ... */).build()?;
//! let (task, spec) = solti_discover::sync(cfg)?;
//! supervisor.submit_with_task(task, &spec).await?;
//! ```
//!
//! ## Also
//!
//! - `DiscoverConfig` / `DiscoverConfigBuilder` (feature `grpc`/`http`): identity, endpoint, transport, timeouts, capabilities.
//! - `sync` (feature `grpc`/`http`): factory returning `Result<(TaskRef, TaskSpec), DiscoverError>`.
//! - [`DiscoverError`]: config, transport, parse, and rejection failures.

#![forbid(unsafe_code)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
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
