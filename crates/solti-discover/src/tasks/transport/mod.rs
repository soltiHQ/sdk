//! Transport selection and dispatch for discovery sync.

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

/// The single transport adapter selected for one discovery task.
pub(super) enum TransportAdapter {
    #[cfg(feature = "grpc")]
    Grpc(Box<GrpcAdapter>),
    #[cfg(feature = "http")]
    Http(HttpAdapter),
}

impl TransportAdapter {
    /// Instantiate only the adapter selected by the validated configuration.
    pub(super) fn from_config(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        match &config.transport {
            #[cfg(feature = "grpc")]
            DiscoveryTransport::Grpc => Ok(Self::Grpc(Box::new(GrpcAdapter::new(config)))),
            #[cfg(feature = "http")]
            DiscoveryTransport::Http => HttpAdapter::new(config).map(Self::Http),
        }
    }

    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.sync(request).await,
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.sync(request).await,
        }
    }
}

/// Turn a `success=false` response into [`DiscoverError::Rejected`].
///
/// `reason` is propagated verbatim from the control plane and remains
/// untrusted server text.
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
            AgentId::from("agent-1"),
            "agent-1",
            "http://127.0.0.1:8085",
            "http://127.0.0.1:9000",
            transport,
            30_000,
            1,
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
    fn validate_response_accepts_success() {
        let response = SyncResponse {
            success: true,
            reason: String::new(),
            retry_after_s: 0,
        };

        assert!(validate_response(response).is_ok());
    }

    #[test]
    fn validate_response_uses_default_reason() {
        let response = SyncResponse {
            success: false,
            reason: String::new(),
            retry_after_s: 0,
        };

        match validate_response(response) {
            Err(DiscoverError::Rejected {
                reason,
                retry_after_s,
            }) => {
                assert!(reason.contains("success=false"));
                assert_eq!(retry_after_s, None);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn validate_response_preserves_positive_retry_hint() {
        let response = SyncResponse {
            success: false,
            reason: "overloaded".into(),
            retry_after_s: 60,
        };

        match validate_response(response) {
            Err(DiscoverError::Rejected {
                reason,
                retry_after_s,
            }) => {
                assert_eq!(reason, "overloaded");
                assert_eq!(retry_after_s, Some(60));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn validate_response_drops_non_positive_retry_hint() {
        let response = SyncResponse {
            success: false,
            reason: "bad".into(),
            retry_after_s: -5,
        };

        match validate_response(response) {
            Err(DiscoverError::Rejected { retry_after_s, .. }) => {
                assert_eq!(retry_after_s, None);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
