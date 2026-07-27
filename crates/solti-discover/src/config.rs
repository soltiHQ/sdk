//! Discovery configuration.

use std::collections::HashMap;

use solti_model::{AgentCapabilities, AgentId, BackoffPolicy, Token};

use crate::errors::DiscoverError;
use crate::metrics::{DiscoverMetricsHandle, noop_discover_metrics};
use crate::proto::EndpointType;

/// Default connect timeout in milliseconds.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
/// Default request timeout in milliseconds.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Transport exposed by the agent API.
///
/// This describes how the control plane reaches the agent. It is independent
/// from [`DiscoveryTransport`], which describes the outbound sync connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEndpointType {
    /// gRPC agent API.
    Grpc,
    /// HTTP agent API.
    Http,
}

impl AgentEndpointType {
    pub(crate) fn as_proto(self) -> i32 {
        match self {
            Self::Grpc => EndpointType::Grpc as i32,
            Self::Http => EndpointType::Http as i32,
        }
    }
}

/// Agent API endpoint advertised to the control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEndpoint {
    pub(crate) address: String,
    pub(crate) endpoint_type: AgentEndpointType,
    pub(crate) api_version: i32,
}

impl AgentEndpoint {
    /// Creates an advertised agent endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoverError::InvalidConfig`] when the address is empty or
    /// `api_version` does not fit the discovery v1 wire field.
    pub fn new(
        address: impl Into<String>,
        endpoint_type: AgentEndpointType,
        api_version: u32,
    ) -> Result<Self, DiscoverError> {
        let address = address.into().trim().to_string();
        if address.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "agent endpoint must not be empty".into(),
            ));
        }
        if api_version == 0 {
            return Err(DiscoverError::InvalidConfig(
                "agent API version must be greater than zero".into(),
            ));
        }
        let api_version = i32::try_from(api_version).map_err(|_| {
            DiscoverError::InvalidConfig("agent API version exceeds the wire range".into())
        })?;

        Ok(Self {
            address,
            endpoint_type,
            api_version,
        })
    }

    /// Returns the advertised address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Returns the advertised transport.
    pub fn endpoint_type(&self) -> AgentEndpointType {
        self.endpoint_type
    }

    /// Returns the advertised agent API version.
    pub fn api_version(&self) -> u32 {
        self.api_version as u32
    }
}

/// Transport used by the agent to sync with the control plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryTransport {
    /// Sync over gRPC.
    #[cfg(feature = "grpc")]
    Grpc,
    /// Sync over HTTP/JSON.
    #[cfg(feature = "http")]
    Http,
}

/// Control-plane endpoint used for discovery sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlPlaneEndpoint {
    pub(crate) address: String,
    pub(crate) transport: DiscoveryTransport,
}

impl ControlPlaneEndpoint {
    /// Creates a control-plane endpoint.
    ///
    /// Transport-specific URI validation happens when the sync task creates
    /// its adapter.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoverError::InvalidConfig`] when the address is empty.
    pub fn new(
        address: impl Into<String>,
        transport: DiscoveryTransport,
    ) -> Result<Self, DiscoverError> {
        let address = address.into().trim().trim_end_matches('/').to_string();
        if address.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "control-plane endpoint must not be empty".into(),
            ));
        }

        Ok(Self { address, transport })
    }

    /// Returns the control-plane address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Returns the outbound discovery transport.
    pub fn transport(&self) -> DiscoveryTransport {
        self.transport
    }
}

/// Validated settings for the discovery sync task.
#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    pub(crate) agent_id: AgentId,
    pub(crate) name: String,
    pub(crate) agent_endpoint: AgentEndpoint,
    pub(crate) control_plane: ControlPlaneEndpoint,
    pub(crate) delay_ms: u64,
    pub(crate) heartbeat_interval_s: i32,
    pub(crate) metadata: HashMap<String, String>,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) token: Option<Token>,
    pub(crate) backoff: Option<BackoffPolicy>,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) task_revision: String,
    pub(crate) metrics: DiscoverMetricsHandle,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<solti_tls::ClientTlsConfig>,
}

impl DiscoverConfig {
    /// Starts a discovery config builder.
    ///
    /// `task_revision` identifies the complete captured runtime intent. Change
    /// it whenever a config value used by the embedded task changes.
    pub fn builder(
        agent_id: AgentId,
        name: impl Into<String>,
        agent_endpoint: AgentEndpoint,
        control_plane: ControlPlaneEndpoint,
        delay_ms: u64,
        task_revision: impl Into<String>,
    ) -> DiscoverConfigBuilder {
        DiscoverConfigBuilder {
            agent_id,
            name: name.into(),
            agent_endpoint,
            control_plane,
            delay_ms,
            metadata: HashMap::new(),
            capabilities: AgentCapabilities::default(),
            token: None,
            backoff: None,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            task_revision: task_revision.into(),
            metrics: noop_discover_metrics(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// Builder for [`DiscoverConfig`].
#[derive(Debug, Clone)]
pub struct DiscoverConfigBuilder {
    agent_id: AgentId,
    name: String,
    agent_endpoint: AgentEndpoint,
    control_plane: ControlPlaneEndpoint,
    delay_ms: u64,
    metadata: HashMap<String, String>,
    capabilities: AgentCapabilities,
    token: Option<Token>,
    backoff: Option<BackoffPolicy>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    task_revision: String,
    metrics: DiscoverMetricsHandle,
    #[cfg(feature = "tls")]
    tls: Option<solti_tls::ClientTlsConfig>,
}

impl DiscoverConfigBuilder {
    /// Sets metadata sent with every sync.
    pub fn metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Sets the registered runner capabilities sent with every sync.
    pub fn capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Overrides the default retry backoff.
    pub fn backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = Some(backoff);
        self
    }

    /// Sets the transport connect timeout in milliseconds.
    pub fn connect_timeout_ms(mut self, ms: u64) -> Self {
        self.connect_timeout_ms = ms;
        self
    }

    /// Sets the request timeout in milliseconds.
    pub fn request_timeout_ms(mut self, ms: u64) -> Self {
        self.request_timeout_ms = ms;
        self
    }

    /// Sets the metrics backend.
    pub fn with_metrics(mut self, metrics: DiscoverMetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }

    /// Sets the bearer token sent with every sync.
    pub fn with_token(mut self, token: Token) -> Self {
        self.token = Some(token);
        self
    }

    /// Sets custom server roots and an optional client identity.
    ///
    /// The control-plane endpoint must use `https`. Without this setting, HTTP
    /// uses platform roots. gRPC uses platform roots when the `tls` feature is
    /// enabled.
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, tls: solti_tls::ClientTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Validates and builds the config.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoverError::InvalidConfig`] when a required value is empty
    /// or a duration cannot be represented by discovery v1.
    pub fn build(self) -> Result<DiscoverConfig, DiscoverError> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "name must not be empty".into(),
            ));
        }
        if self.delay_ms == 0 {
            return Err(DiscoverError::InvalidConfig(
                "delay_ms must be greater than zero".into(),
            ));
        }
        if self.connect_timeout_ms == 0 {
            return Err(DiscoverError::InvalidConfig(
                "connect_timeout_ms must be greater than zero".into(),
            ));
        }
        if self.request_timeout_ms == 0 {
            return Err(DiscoverError::InvalidConfig(
                "request_timeout_ms must be greater than zero".into(),
            ));
        }
        let task_revision = self.task_revision.trim().to_string();
        if task_revision.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "task_revision must not be empty".into(),
            ));
        }
        let heartbeat_interval_s = i32::try_from(self.delay_ms.div_ceil(1_000)).map_err(|_| {
            DiscoverError::InvalidConfig("delay_ms exceeds the discovery v1 wire range".into())
        })?;

        Ok(DiscoverConfig {
            agent_id: self.agent_id,
            name,
            agent_endpoint: self.agent_endpoint,
            control_plane: self.control_plane,
            delay_ms: self.delay_ms,
            heartbeat_interval_s,
            metadata: self.metadata,
            capabilities: self.capabilities,
            token: self.token,
            backoff: self.backoff,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            task_revision,
            metrics: self.metrics,
            #[cfg(feature = "tls")]
            tls: self.tls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "http")]
    fn config(delay_ms: u64) -> Result<DiscoverConfig, DiscoverError> {
        DiscoverConfig::builder(
            AgentId::new("agent-1").unwrap(),
            "agent-1",
            AgentEndpoint::new("http://127.0.0.1:8085", AgentEndpointType::Http, 1)?,
            ControlPlaneEndpoint::new("http://127.0.0.1:9000", DiscoveryTransport::Http)?,
            delay_ms,
            "test@1",
        )
        .build()
    }

    #[test]
    fn advertised_transport_is_independent_from_compiled_discovery_transport() {
        let grpc = AgentEndpoint::new("127.0.0.1:50051", AgentEndpointType::Grpc, 1).unwrap();
        let http = AgentEndpoint::new("http://127.0.0.1:8085", AgentEndpointType::Http, 1).unwrap();

        assert_eq!(grpc.endpoint_type(), AgentEndpointType::Grpc);
        assert_eq!(http.endpoint_type(), AgentEndpointType::Http);
    }

    #[test]
    fn agent_api_version_must_fit_wire_type() {
        assert!(AgentEndpoint::new("127.0.0.1:8085", AgentEndpointType::Http, 0).is_err());
        assert!(
            AgentEndpoint::new(
                "127.0.0.1:8085",
                AgentEndpointType::Http,
                i32::MAX as u32 + 1,
            )
            .is_err()
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn heartbeat_interval_rounds_up() {
        assert_eq!(config(1).unwrap().heartbeat_interval_s, 1);
        assert_eq!(config(1_000).unwrap().heartbeat_interval_s, 1);
        assert_eq!(config(1_001).unwrap().heartbeat_interval_s, 2);
    }

    #[cfg(feature = "http")]
    #[test]
    fn runtime_revision_is_required() {
        let result = DiscoverConfig::builder(
            AgentId::new("agent-1").unwrap(),
            "agent-1",
            AgentEndpoint::new("127.0.0.1:8085", AgentEndpointType::Http, 1).unwrap(),
            ControlPlaneEndpoint::new("http://127.0.0.1:9000", DiscoveryTransport::Http).unwrap(),
            1_000,
            " ",
        )
        .build();

        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }
}
