//! # Discovery configuration.
//!
//! [`DiscoverConfig`] captures agent identity, control-plane endpoint, transport choice, and sync interval.
//! Passed to [`sync`](crate::sync) to build the heartbeat task.

use std::collections::HashMap;

use solti_model::AgentId;

use crate::proto::EndpointType;

/// Transport protocol for the sync RPC.
///
/// ## Also
///
/// - [`DiscoverConfig`] uses this to select gRPC or HTTP path.
/// - `proto::EndpointType` is the wire-level equivalent.
#[derive(Clone, Debug)]
pub enum DiscoveryTransport {
    Grpc,
    Http,
}

impl DiscoveryTransport {
    pub fn as_proto(&self) -> i32 {
        match self {
            DiscoveryTransport::Grpc => EndpointType::Grpc as i32,
            DiscoveryTransport::Http => EndpointType::Http as i32,
        }
    }
}

/// Agent discovery settings.
///
/// ## Also
///
/// - [`DiscoveryTransport`] selects gRPC or HTTP.
/// - [`DiscoverError`](crate::DiscoverError) failure modes during sync.
/// - [`sync`](crate::sync) consumes this config to produce `(TaskRef, TaskSpec)`.
#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    /// Unique agent identifier (provided by caller).
    pub agent_id: AgentId,
    pub metadata: HashMap<String, String>,
    pub control_plane_endpoint: String,
    pub transport: DiscoveryTransport,
    pub agent_endpoint: String,
    pub name: String,
    pub delay_ms: u64,
    /// Agent capabilities: features the agent supports beyond the base API.
    /// Known values: `"task_runs"`, `"task_delete"`, `"cancel"`.
    pub capabilities: Vec<String>,
}
