//! # solti-discover
//!
//! Discovery task for a Solti agent.
//!
//! The crate builds one embedded periodic task.
//! The task advertises the agent and syncs liveness with a control plane.
//!
//! ## Start Here
//!
//! Use `ControlPlaneEndpoint` for the outbound discovery connection.
//! Use `DiscoverConfig` to capture the complete task intent.
//! Use `AgentEndpoint` for the API exposed by the agent.
//! Use `sync` to create the manifest and Taskvisor task.
//!
//! ## Flow
//!
//! ```text
//! agent identity + endpoint + capabilities
//!                    ▼
//!             DiscoverConfig
//!                    │ sync(config, uptime)
//!                    ▼
//!       TaskManifest + embedded TaskRef
//!                    │ repeated attempts
//!                    ▼
//!          HTTP or gRPC control plane
//! ```
//!
//! The advertised agent endpoint and discovery transport are independent.
//! An HTTP agent can sync through gRPC.
//! A gRPC agent can sync through HTTP.
//!
//! ## Credential Transport
//!
//! A configured bearer token requires an HTTPS control-plane endpoint by default.
//! Plaintext HTTP and gRPC remain available when no token is configured.
//! `DiscoverConfigBuilder::allow_insecure_token_transport` is an explicit
//! development or loopback escape hatch.
//!
//! ## Retry Model
//!
//! [`DiscoverError::retryability`] classifies every failure.
//! Retryable failures become `TaskError::Fail`.
//! Permanent failures become `TaskError::Fatal`.
//!
//! A rejected response may request a hold through `retry_after_s`.
//! The task caps that hold at one hour.
//! Taskvisor schedules retry backoff after the failed attempt.
//! Any remaining server hold is awaited before the next request.
//!
//! ## Time Units
//!
//! Public duration arguments ending in `_ms` use milliseconds.
//! `retry_after_s` uses seconds because it mirrors discovery protocol v1.
//! Uptime also uses whole seconds.
//!
//! ## Features
//!
//! | Feature | Transport or extension                    |
//! |---------|-------------------------------------------|
//! | `http`  | HTTP/JSON discovery v1                    |
//! | `grpc`  | gRPC discovery v1                         |
//! | `tls`   | Custom TLS and gRPC HTTPS support         |
//!
//! No feature is enabled by default.
//! The base crate exposes error and metrics contracts.
//!
//! ## Main Types
//!
//! | Area             | Types                                                           |
//! |------------------|-----------------------------------------------------------------|
//! | Agent endpoint   | `AgentEndpoint`, `AgentEndpointType`                            |
//! | Control plane    | `ControlPlaneEndpoint`, `DiscoveryTransport`                    |
//! | Configuration    | `DiscoverConfig`, `DiscoverConfigBuilder`                       |
//! | Task factory     | `sync`                                                          |
//! | Uptime           | `UptimeSource`, `MonotonicUptime`                               |
//! | Metrics          | [`DiscoverMetricsBackend`], [`DiscoverFailReason`]              |
//! | Errors           | [`DiscoverError`], [`Retryability`]                             |
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! # #[cfg(feature = "http")]
//! # fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use solti_discover::{
//!     AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
//!     DiscoveryTransport, MonotonicUptime, sync,
//! };
//! use solti_model::AgentId;
//!
//! let config = DiscoverConfig::builder(
//!     AgentId::new("agent-1")?,
//!     "Agent 1",
//!     AgentEndpoint::new(
//!         concat!("http", "://127.0.0.1:8085"),
//!         AgentEndpointType::Http,
//!         1,
//!     )?,
//!     ControlPlaneEndpoint::new(
//!         "https://control.example",
//!         DiscoveryTransport::Http,
//!     )?,
//!     solti_model::AgentCapabilities::default(),
//!     30_000,
//!     "discovery-config@1",
//! )
//! .build()?;
//!
//! let uptime = Arc::new(MonotonicUptime::new());
//! let (manifest, task_ref) = sync(config, uptime)?;
//! # let _ = (manifest, task_ref);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
///
/// The README examples use the `http` and `tls` features.
/// This check runs when both features are enabled.
#[cfg(all(doctest, feature = "http", feature = "tls"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod version;
pub use version::{
    DISCOVERY_GRPC_PACKAGE, DISCOVERY_GRPC_SERVICE, DISCOVERY_HTTP_SYNC_PATH,
    DISCOVERY_PROTOCOL_VERSION,
};

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

mod errors;
pub use errors::{DiscoverError, Retryability};

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
            pub(crate) mod wire {
                include!(concat!(
                    env!("OUT_DIR"),
                    "/solti.discover.v",
                    env!("SOLTI_DISCOVERY_PROTOCOL_MAJOR"),
                    ".rs"
                ));

                #[cfg(feature = "http")]
                include!(concat!(
                    env!("OUT_DIR"),
                    "/solti.discover.v",
                    env!("SOLTI_DISCOVERY_PROTOCOL_MAJOR"),
                    ".serde.rs"
                ));
            }
        }
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) use generated::solti::agent::v1 as proto_agent;
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) use generated::solti::discover::wire as proto;

#[cfg(test)]
mod contract_identity_guard {
    #[test]
    fn discovery_contract_identity_is_consistent() {
        assert_eq!(
            super::DISCOVERY_GRPC_PACKAGE,
            format!("solti.discover.v{}", super::DISCOVERY_PROTOCOL_VERSION),
        );
        assert_eq!(
            super::DISCOVERY_GRPC_SERVICE,
            format!("{}.DiscoverService", super::DISCOVERY_GRPC_PACKAGE),
        );
        assert_eq!(
            super::DISCOVERY_HTTP_SYNC_PATH,
            format!("/api/v{}/discovery/sync", super::DISCOVERY_PROTOCOL_VERSION),
        );
    }
}
