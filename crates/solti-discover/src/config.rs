//! # Discovery configuration.
//!
//! [`DiscoverConfig`] captures agent identity, control-plane endpoint, transport choice, and sync interval.
//! Construct via [`DiscoverConfig::builder`] - all fields validated on `build()`.

use std::collections::HashMap;

use solti_model::{AgentId, BackoffPolicy, Token};

#[cfg(any(feature = "grpc", feature = "http"))]
use crate::proto::EndpointType;

use crate::errors::DiscoverError;
use crate::metrics::{DiscoverMetricsHandle, noop_discover_metrics};

/// Default connect timeout in milliseconds.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
/// Default request timeout in milliseconds.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Transport protocol for the sync RPC.
///
/// Variants are gated by crate features - a crate built without any transport feature exposes an empty enum (and no `sync` factory).
///
/// ## Also
///
/// - [`DiscoverConfig`] uses this to select gRPC or HTTP path.
/// - `proto::EndpointType` is the wire-level equivalent.
#[derive(Clone, Debug)]
pub enum DiscoveryTransport {
    /// Sync over gRPC (`DiscoverService.Sync`, tonic channel).
    #[cfg(feature = "grpc")]
    Grpc,
    /// Sync over HTTP/JSON (`POST /api/v{n}/discovery/sync`, reqwest client).
    #[cfg(feature = "http")]
    Http,
}

impl DiscoveryTransport {
    #[cfg(any(feature = "grpc", feature = "http"))]
    pub(crate) fn as_proto(&self) -> i32 {
        match self {
            #[cfg(feature = "grpc")]
            DiscoveryTransport::Grpc => EndpointType::Grpc as i32,
            #[cfg(feature = "http")]
            DiscoveryTransport::Http => EndpointType::Http as i32,
        }
    }
}

/// Agent discovery settings.
///
/// Construct via [`DiscoverConfig::builder`].
/// All invariants are enforced at build time.
///
/// ## Also
///
/// - [`DiscoveryTransport`] selects gRPC or HTTP.
/// - [`DiscoverError`] failure modes during sync.
/// - [`sync`](crate::sync) consumes this config to produce `(TaskRef, TaskSpec)`.
#[derive(Debug, Clone)]
pub struct DiscoverConfig {
    pub(crate) agent_id: AgentId,
    pub(crate) name: String,
    pub(crate) agent_endpoint: String,
    pub(crate) control_plane_endpoint: String,
    pub(crate) transport: DiscoveryTransport,
    pub(crate) delay_ms: u64,
    pub(crate) api_version: u32,
    pub(crate) metadata: HashMap<String, String>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) token: Option<Token>,
    pub(crate) backoff: Option<BackoffPolicy>,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) metrics: DiscoverMetricsHandle,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<solti_tls::ClientTlsConfig>,
}

impl DiscoverConfig {
    /// Start a builder for `DiscoverConfig`.
    pub fn builder(
        agent_id: AgentId,
        name: impl Into<String>,
        agent_endpoint: impl Into<String>,
        control_plane_endpoint: impl Into<String>,
        transport: DiscoveryTransport,
        delay_ms: u64,
        api_version: u32,
    ) -> DiscoverConfigBuilder {
        DiscoverConfigBuilder {
            agent_id,
            name: name.into(),
            agent_endpoint: agent_endpoint.into(),
            control_plane_endpoint: control_plane_endpoint.into(),
            transport,
            delay_ms,
            api_version,
            metadata: HashMap::new(),
            capabilities: Vec::new(),
            token: None,
            backoff: None,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            metrics: noop_discover_metrics(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// Validated builder for [`DiscoverConfig`].
#[derive(Debug, Clone)]
pub struct DiscoverConfigBuilder {
    agent_id: AgentId,
    name: String,
    agent_endpoint: String,
    control_plane_endpoint: String,
    transport: DiscoveryTransport,
    delay_ms: u64,
    api_version: u32,
    metadata: HashMap<String, String>,
    capabilities: Vec<String>,
    token: Option<Token>,
    backoff: Option<BackoffPolicy>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    metrics: DiscoverMetricsHandle,
    #[cfg(feature = "tls")]
    tls: Option<solti_tls::ClientTlsConfig>,
}

impl DiscoverConfigBuilder {
    /// Attach user metadata sent with every sync.
    pub fn metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Declare agent capabilities (see `proto/solti/discover/v1/discovery.proto` for known values).
    pub fn capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Override the default backoff policy.
    ///
    /// Default (when not set): equal jitter, `first_ms = delay_ms/2`, `max_ms = delay_ms*3`, factor 2.0.
    pub fn backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = Some(backoff);
        self
    }

    /// Transport-level connect timeout (ms).
    pub fn connect_timeout_ms(mut self, ms: u64) -> Self {
        self.connect_timeout_ms = ms;
        self
    }

    /// End-to-end request timeout (ms).
    pub fn request_timeout_ms(mut self, ms: u64) -> Self {
        self.request_timeout_ms = ms;
        self
    }

    /// Attach a metrics backend. When not set, a zero-cost no-op is used.
    pub fn with_metrics(mut self, metrics: DiscoverMetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }

    /// Attach a bearer token presented to the control plane on every sync.
    ///
    /// Transport-agnostic:
    ///  - HTTP sends it as `Authorization: Bearer <token>`,
    ///  - gRPC sends the same value as `authorization` metadata.
    pub fn with_token(mut self, token: Token) -> Self {
        self.token = Some(token);
        self
    }

    /// Enable TLS / mTLS for the sync transport.
    ///
    /// Both gRPC and HTTP transports honour this configuration:
    /// the control-plane endpoint must be reachable over `https://` (HTTP) or use `https://`/`tls://` semantics in `tonic` (gRPC).
    ///
    /// Available with the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, tls: solti_tls::ClientTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Validate and produce a [`DiscoverConfig`].
    ///
    /// Trims trailing `/` from `control_plane_endpoint`.
    ///
    /// ## Errors
    ///
    /// - [`DiscoverError::InvalidConfig`]: `name`, `agent_endpoint`, or `control_plane_endpoint` is empty;
    ///   `delay_ms`, `api_version`, `connect_timeout_ms`, or `request_timeout_ms` is zero;
    ///   a token is set but empty.
    pub fn build(self) -> Result<DiscoverConfig, DiscoverError> {
        if self.name.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "name must not be empty".into(),
            ));
        }
        if self.agent_endpoint.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "agent_endpoint must not be empty".into(),
            ));
        }
        if self.control_plane_endpoint.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "control_plane_endpoint must not be empty".into(),
            ));
        }
        if self.delay_ms == 0 {
            return Err(DiscoverError::InvalidConfig("delay_ms must be > 0".into()));
        }
        if self.api_version == 0 {
            return Err(DiscoverError::InvalidConfig(
                "api_version must be > 0".into(),
            ));
        }
        if self.connect_timeout_ms == 0 {
            return Err(DiscoverError::InvalidConfig(
                "connect_timeout_ms must be > 0".into(),
            ));
        }
        if self.request_timeout_ms == 0 {
            return Err(DiscoverError::InvalidConfig(
                "request_timeout_ms must be > 0".into(),
            ));
        }
        if let Some(token) = &self.token
            && token.is_empty()
        {
            return Err(DiscoverError::InvalidConfig(
                "token must not be empty".into(),
            ));
        }

        let control_plane_endpoint = self
            .control_plane_endpoint
            .trim_end_matches('/')
            .to_string();

        Ok(DiscoverConfig {
            agent_id: self.agent_id,
            name: self.name,
            agent_endpoint: self.agent_endpoint,
            control_plane_endpoint,
            transport: self.transport,
            delay_ms: self.delay_ms,
            api_version: self.api_version,
            metadata: self.metadata,
            capabilities: self.capabilities,
            token: self.token,
            backoff: self.backoff,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            metrics: self.metrics,
            #[cfg(feature = "tls")]
            tls: self.tls,
        })
    }
}
