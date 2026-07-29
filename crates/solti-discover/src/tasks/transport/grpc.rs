//! # gRPC discovery
//!
//! ```text
//! SyncRequest
//!      │ tonic Request
//!      ▼
//! lazy shared channel
//!      │ DiscoverService.Sync
//!      ▼
//! SyncResponse ──► protocol validation
//! ```
//!
//! Connection starts on the first sync attempt.
//! A successful channel is reused.
//! HTTPS requires the `tls` feature.

use std::time::Duration;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use crate::config::DiscoverConfig;
use crate::errors::DiscoverError;
use crate::proto::SyncRequest;
use crate::proto::discover_service_client::DiscoverServiceClient;
use crate::tasks::transport::validate_response;

/// gRPC discovery adapter.
pub(in crate::tasks) struct GrpcAdapter {
    authorization: Option<MetadataValue<Ascii>>,
    client: tokio::sync::OnceCell<DiscoverServiceClient<Channel>>,
    endpoint: Endpoint,
    secure: bool,
}

impl GrpcAdapter {
    /// Creates an adapter from the discovery config.
    pub(super) fn new(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
        let mut endpoint = Endpoint::from_shared(config.control_plane.address.clone())
            .map_err(|error| {
                DiscoverError::InvalidConfig(format!(
                    "invalid gRPC control-plane endpoint: {error}"
                ))
            })?
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms));

        let scheme = endpoint.uri().scheme_str();
        if !matches!(scheme, Some("http" | "https")) {
            return Err(DiscoverError::InvalidConfig(
                "gRPC control-plane endpoint must use http or https".into(),
            ));
        }
        let secure = scheme == Some("https");

        #[cfg(feature = "tls")]
        {
            if config.tls.is_some() && !secure {
                return Err(DiscoverError::InvalidConfig(
                    "custom TLS requires an https control-plane endpoint".into(),
                ));
            }
            if secure {
                let tls = build_tonic_client_tls(select_client_tls(config))?;
                endpoint = endpoint.tls_config(tls).map_err(|error| {
                    DiscoverError::InvalidConfig(format!(
                        "configure gRPC TLS: {}",
                        error_chain(&error)
                    ))
                })?;
            }
        }

        #[cfg(not(feature = "tls"))]
        if secure {
            return Err(DiscoverError::InvalidConfig(
                "https gRPC discovery requires the tls feature".into(),
            ));
        }

        Ok(Self {
            authorization: authorization_metadata(config.token.as_ref())?,
            client: tokio::sync::OnceCell::new(),
            endpoint,
            secure,
        })
    }

    /// Sends one discovery request.
    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        let client = self
            .client
            .get_or_try_init(|| async {
                let channel = self.endpoint.connect().await?;
                Ok::<_, DiscoverError>(DiscoverServiceClient::new(channel))
            })
            .await?;

        let mut client = client.clone();
        let mut request = tonic::Request::new(request);
        if let Some(value) = &self.authorization {
            request
                .metadata_mut()
                .insert("authorization", value.clone());
        }

        match client.sync(request).await {
            Ok(response) => validate_response(response.into_inner()),
            Err(status) if is_auth_status(status.code()) => Err(DiscoverError::AuthFailed {
                reason: format!("grpc {:?}: {}", status.code(), status.message()),
            }),
            Err(status) => Err(DiscoverError::from(status)),
        }
    }

    /// Returns whether the endpoint uses HTTPS.
    pub(super) fn is_secure(&self) -> bool {
        self.secure
    }
}

/// Encodes an optional bearer token.
fn authorization_metadata(
    token: Option<&solti_model::Token>,
) -> Result<Option<MetadataValue<Ascii>>, DiscoverError> {
    token
        .map(|token| {
            format!("Bearer {}", token.expose()).parse().map_err(|_| {
                DiscoverError::InvalidConfig(
                    "token contains characters invalid for an Authorization header".into(),
                )
            })
        })
        .transpose()
}

/// Returns whether a gRPC status represents authentication failure.
fn is_auth_status(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
    )
}

#[cfg(feature = "tls")]
/// TLS source selected for one gRPC adapter.
enum ClientTls<'a> {
    NativeRoots,
    Custom(&'a solti_tls::ClientTlsConfig),
}

#[cfg(feature = "tls")]
/// Selects native roots or custom TLS.
fn select_client_tls(config: &DiscoverConfig) -> ClientTls<'_> {
    match config.tls.as_ref() {
        Some(config) => ClientTls::Custom(config),
        None => ClientTls::NativeRoots,
    }
}

#[cfg(feature = "tls")]
/// Builds tonic TLS settings.
fn build_tonic_client_tls(
    config: ClientTls<'_>,
) -> Result<tonic::transport::ClientTlsConfig, DiscoverError> {
    match config {
        ClientTls::NativeRoots => Ok(tonic::transport::ClientTlsConfig::new().with_native_roots()),
        ClientTls::Custom(config) => {
            let loaded = config.clone().load().map_err(|error| {
                DiscoverError::InvalidConfig(format!("load TLS config: {error}"))
            })?;
            Ok(tonic_client_tls_from_loaded(&loaded))
        }
    }
}

#[cfg(feature = "tls")]
/// Converts loaded Solti TLS material into tonic settings.
fn tonic_client_tls_from_loaded(
    config: &solti_tls::LoadedClientTlsConfig,
) -> tonic::transport::ClientTlsConfig {
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};

    let mut tls =
        ClientTlsConfig::new().ca_certificate(Certificate::from_pem(config.server_roots_pem()));
    if let Some(identity) = config.identity() {
        tls = tls.identity(Identity::from_pem(
            identity.certificate_chain_pem(),
            identity.expose_private_key_pem(),
        ));
    }
    tls
}

#[cfg(feature = "tls")]
/// Formats an error and every source in its chain.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut messages = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current {
        messages.push(error.to_string());
        current = error.source();
    }
    messages.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_builder(endpoint: impl Into<String>) -> crate::DiscoverConfigBuilder {
        DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(endpoint, crate::DiscoveryTransport::Grpc).unwrap(),
            30_000,
            "test@1",
        )
    }

    fn config(endpoint: impl Into<String>) -> DiscoverConfig {
        config_builder(endpoint).build().expect("config builds")
    }

    #[test]
    fn endpoint_is_validated_before_task_execution() {
        let result = GrpcAdapter::new(&config("not a URI"));
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[cfg(not(feature = "tls"))]
    #[test]
    fn https_requires_tls_feature() {
        let result = GrpcAdapter::new(&config("https://control.example"));
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[cfg(feature = "tls")]
    #[test]
    fn https_without_custom_roots_selects_native_roots() {
        let config = config("https://control.example");
        assert!(matches!(select_client_tls(&config), ClientTls::NativeRoots));
    }

    #[cfg(feature = "tls")]
    #[test]
    fn custom_tls_requires_https() {
        let tls = solti_tls::ClientTlsConfig::new(solti_tls::TrustRoots::from_pem_bytes(
            b"not loaded because the scheme is invalid".to_vec(),
        ));
        let config = config_builder("http://control.example")
            .with_tls(tls)
            .build()
            .unwrap();

        let result = GrpcAdapter::new(&config);
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[test]
    fn token_is_validated_before_task_execution() {
        let config = config_builder("http://control.example")
            .with_token(solti_model::Token::new("first\nsecond").unwrap())
            .build()
            .unwrap();

        let result = GrpcAdapter::new(&config);
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[cfg(feature = "tls")]
    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct OuterError {
        #[source]
        source: std::io::Error,
    }

    #[cfg(feature = "tls")]
    #[test]
    fn error_chain_keeps_the_transport_source() {
        let error = OuterError {
            source: std::io::Error::other("inner"),
        };
        assert_eq!(error_chain(&error), "outer: inner");
    }
}
