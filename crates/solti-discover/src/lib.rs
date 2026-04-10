//! # Agent discovery and heartbeat.
//!
//! Periodic sync task that registers an agent with the control plane
//! and reports liveness, platform telemetry, and capabilities.
//!
//! | transport | module                         | protocol                      |
//! |-----------|--------------------------------|-------------------------------|
//! | gRPC      | `proto::DiscoverServiceClient` | `proto/v1/sync.proto`         |
//! | HTTP      | `reqwest::Client`              | `POST /api/v1/discovery/sync` |
//!
//! ## Quick start
//!
//! ```text
//! let (task, spec) = solti_discover::sync(config);
//! supervisor.submit_with_task(task, &spec).await?;
//! ```
//!
//! ## Also
//!
//! - [`DiscoverConfig`] agent identity, endpoint, transport, capabilities.
//! - [`DiscoverError`] all failure modes (transport, parse, rejection).
//! - [`sync`] factory that returns `(TaskRef, TaskSpec)` for the supervisor.

pub(crate) mod proto {
    tonic::include_proto!("solti.discover.v1");
}
pub use proto::{SyncRequest, SyncResponse};

mod config;
pub use config::DiscoveryTransport;
pub use config::DiscoverConfig;

mod errors;
pub use errors::DiscoverError;

mod tasks;
pub use tasks::sync;
