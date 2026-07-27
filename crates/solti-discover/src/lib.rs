//! Agent registration and heartbeat.
//!
//! `solti-discover` builds one embedded periodic task. The task advertises the
//! agent API and syncs liveness data with a control plane.
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
//! use std::sync::Arc;
//! use solti_discover::{
//!     AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
//!     DiscoveryTransport, MonotonicUptime,
//! };
//! use solti_model::AgentId;
//!
//! let uptime = Arc::new(MonotonicUptime::new());
//!
//! let cfg = DiscoverConfig::builder(
//!     AgentId::new("agent-1")?,
//!     "agent-1",               // display name
//!     AgentEndpoint::new(
//!         "http://127.0.0.1:8085",
//!         AgentEndpointType::Http,
//!         1,
//!     )?,
//!     ControlPlaneEndpoint::new(
//!         "http://127.0.0.1:9000",
//!         DiscoveryTransport::Http,
//!     )?,
//!     30_000,
//!     "agent@1",
//! )
//! .build()?;
//!
//! let (manifest, task_ref) = solti_discover::sync(cfg, uptime)?;
//! // Submit to a running Solti supervisor:
//! // supervisor.create_embedded_task(manifest, task_ref).await?;
//! # let _ = (manifest, task_ref);
//! # Ok(())
//! # }
//! ```
//!
//! ## Time units
//!
//! Every public numeric duration in this crate is in **milliseconds** and carries an `_ms` suffix
//! (`delay_ms`, `connect_timeout_ms`, `request_timeout_ms`).
//! The one seconds-valued exception is `retry_after_s` on [`DiscoverError::Rejected`]:
//! it mirrors the control-plane wire contract verbatim
//! (field `retry_after_s` in `proto/solti/discover/v1/discovery.proto`).
//!
//! [`AgentEndpoint`] describes the inbound agent API. [`ControlPlaneEndpoint`]
//! describes the outbound discovery connection. These transports are independent.

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
pub use errors::{DiscoverError, Retryability};

mod metrics;
pub use metrics::{
    DiscoverFailReason, DiscoverMetricsBackend, DiscoverMetricsHandle, NoOpDiscoverMetrics,
    OUTCOME_FAILURE, OUTCOME_SUCCESS, noop_discover_metrics,
};

#[cfg(any(feature = "grpc", feature = "http"))]
mod config;
#[cfg(any(feature = "grpc", feature = "http"))]
pub use config::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DEFAULT_CONNECT_TIMEOUT_MS,
    DEFAULT_REQUEST_TIMEOUT_MS, DiscoverConfig, DiscoverConfigBuilder, DiscoveryTransport,
};

#[cfg(any(feature = "grpc", feature = "http"))]
mod tasks;
#[cfg(any(feature = "grpc", feature = "http"))]
pub use tasks::sync;

#[cfg(any(feature = "grpc", feature = "http"))]
mod uptime;
#[cfg(any(feature = "grpc", feature = "http"))]
pub use uptime::{MonotonicUptime, UptimeSource};

#[cfg(any(feature = "grpc", feature = "http"))]
mod generated {
    pub(crate) mod solti {
        pub(crate) mod agent {
            pub(crate) mod v1 {
                include!(concat!(env!("OUT_DIR"), "/solti.agent.v1.rs"));

                #[cfg(feature = "http")]
                include!(concat!(env!("OUT_DIR"), "/solti.agent.v1.serde.rs"));
            }
        }

        pub(crate) mod discover {
            pub(crate) mod v1 {
                include!(concat!(env!("OUT_DIR"), "/solti.discover.v1.rs"));

                #[cfg(feature = "http")]
                include!(concat!(env!("OUT_DIR"), "/solti.discover.v1.serde.rs"));
            }
        }
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) use generated::solti::agent::v1 as proto_agent;
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) use generated::solti::discover::v1 as proto;
