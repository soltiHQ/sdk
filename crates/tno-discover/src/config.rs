use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum DiscoveryTransport {
    Grpc,
    Http,
}

#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    pub metadata: HashMap<String, String>,
    pub control_plane_endpoint: String,
    pub transport: DiscoveryTransport,
    pub agent_endpoint: String,
    pub name: String,
    pub delay_ms: u64,
}
