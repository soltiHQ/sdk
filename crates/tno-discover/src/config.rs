use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum DiscoveryTransport {
    Grpc,
    Http,
}

#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    pub name: String,
    pub endpoint: String,
    pub transport: DiscoveryTransport,
    pub metadata: HashMap<String, String>,
    pub delay_ms: u64,
}
