//! # Discovery transport
//!
//! [`TransportAdapter`] owns exactly one selected transport.
//!
//! ```text
//! DiscoverConfig.control_plane.transport
//!                  ▼
//!          TransportAdapter
//!             ├──► HTTP adapter
//!             └──► gRPC adapter
//!                  ▼
//!             SyncResponse
//!                  │ validate_response
//!                  ▼
//!           Ok or DiscoverError
//! ```
//!
//! Adapter construction validates transport-specific URI and authentication data.
//! A bearer token requires TLS unless the config explicitly opts into plaintext transport.
//! The transport client is reused across task attempts.

#[cfg(feature = "grpc")]
mod grpc;
#[cfg(feature = "http")]
mod http;

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;
use crate::proto::{SyncRequest, SyncResponse};

#[cfg(feature = "grpc")]
use grpc::GrpcAdapter;
#[cfg(feature = "http")]
use http::HttpAdapter;

/// Selected transport for one discovery task.
pub(super) enum TransportAdapter {
    #[cfg(feature = "grpc")]
    Grpc(Box<GrpcAdapter>),
    #[cfg(feature = "http")]
    Http(HttpAdapter),
}

impl TransportAdapter {
    /// Creates only the adapter selected by the config.
    pub(super) fn from_config(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        match config.control_plane.transport {
            #[cfg(feature = "grpc")]
            DiscoveryTransport::Grpc => GrpcAdapter::new(config).map(Box::new).map(Self::Grpc),
            #[cfg(feature = "http")]
            DiscoveryTransport::Http => HttpAdapter::new(config).map(Self::Http),
        }
    }

    /// Sends one request through the selected adapter.
    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.sync(request).await,
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.sync(request).await,
        }
    }

    /// Returns whether the selected endpoint uses TLS.
    pub(super) fn is_secure(&self) -> bool {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.is_secure(),
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.is_secure(),
        }
    }
}

/// Enforces the bearer credential transport policy after URI validation.
fn validate_token_transport(config: &DiscoverConfig, secure: bool) -> Result<(), DiscoverError> {
    if config.token.is_some() && !secure && !config.allow_insecure_token_transport {
        return Err(DiscoverError::InvalidConfig(
            "bearer token over plaintext discovery is disabled; use an https control-plane endpoint or explicitly call allow_insecure_token_transport() for development or loopback use"
                .into(),
        ));
    }

    Ok(())
}

/// Converts `success = false` into [`DiscoverError::Rejected`].
///
/// The rejection reason remains untrusted server text.
fn validate_response(response: SyncResponse) -> Result<(), DiscoverError> {
    if response.success {
        return Ok(());
    }

    let reason = if response.reason.is_empty() {
        "control plane returned success=false".to_string()
    } else {
        response.reason
    };
    let retry_after_s = (response.retry_after_s > 0).then_some(response.retry_after_s);

    Err(DiscoverError::Rejected {
        reason,
        retry_after_s,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "grpc", feature = "http"))]
    use solti_model::AgentId;

    use super::*;

    #[cfg(all(feature = "grpc", feature = "http"))]
    fn config(transport: DiscoveryTransport) -> DiscoverConfig {
        DiscoverConfig::builder(
            AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new("http://127.0.0.1:9000", transport).unwrap(),
            30_000,
            "test@1",
        )
        .build()
        .expect("config builds")
    }

    #[cfg(all(feature = "grpc", feature = "http"))]
    #[test]
    fn instantiates_only_the_selected_transport_adapter() {
        let http = TransportAdapter::from_config(&config(DiscoveryTransport::Http))
            .expect("http adapter builds");
        assert!(matches!(http, TransportAdapter::Http(_)));

        let grpc = TransportAdapter::from_config(&config(DiscoveryTransport::Grpc))
            .expect("grpc adapter builds");
        assert!(matches!(grpc, TransportAdapter::Grpc(_)));
    }

    #[test]
    fn response_validation_preserves_the_wire_contract() {
        assert!(
            validate_response(SyncResponse {
                success: true,
                reason: String::new(),
                retry_after_s: 0,
            })
            .is_ok()
        );

        for (reason, retry_after_s, expected_reason, expected_retry_after_s) in [
            ("", 0, "control plane returned success=false", None),
            ("overloaded", 60, "overloaded", Some(60)),
            ("bad", -5, "bad", None),
        ] {
            let result = validate_response(SyncResponse {
                success: false,
                reason: reason.into(),
                retry_after_s,
            });
            match result {
                Err(DiscoverError::Rejected {
                    reason,
                    retry_after_s,
                }) => {
                    assert_eq!(reason, expected_reason);
                    assert_eq!(retry_after_s, expected_retry_after_s);
                }
                other => panic!("expected Rejected, got {other:?}"),
            }
        }
    }
}
