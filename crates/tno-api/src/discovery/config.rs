#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Unique agent identifier.
    pub agent_id: String,
    /// Agent's own API endpoint (what lighthouse should call back).
    pub agent_endpoint: String,
    /// Lighthouse gRPC endpoint.
    pub lighthouse_endpoint: String,
    /// Heartbeat interval (default: 30 seconds).
    pub heartbeat_interval_ms: u64,
    /// Timeout for heartbeat RPC (default: 5 seconds).
    pub heartbeat_timeout_ms: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            agent_id: hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown-agent".to_string()),
            agent_endpoint: "http://localhost:8080".to_string(),
            lighthouse_endpoint: "http://localhost:50051".to_string(),
            heartbeat_interval_ms: 30_000,
            heartbeat_timeout_ms: 5_000,
        }
    }
}

impl DiscoveryConfig {
    pub fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}
