use std::collections::HashMap;

use solti_model::AgentId;

#[derive(Clone, Debug)]
pub enum DiscoveryTransport {
    Grpc,
    Http,
}

impl DiscoveryTransport {
    pub fn as_proto(&self) -> i32 {
        match self {
            DiscoveryTransport::Grpc => 0,
            DiscoveryTransport::Http => 1,
        }
    }
}

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
}
