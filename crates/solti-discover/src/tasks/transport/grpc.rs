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
use crate::tasks::transport::{validate_response, validate_token_transport};

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

        let authorization = authorization_metadata(config.token.as_ref())?;
        validate_token_transport(config, secure)?;

        Ok(Self {
            authorization,
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
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tonic::codegen::{Body, BoxFuture, Service, StdError, http};
    use tonic::server::{NamedService, UnaryService};
    use tonic::transport::server::TcpIncoming;

    use super::*;

    #[derive(Clone)]
    struct TestDiscoverServer {
        expected_authorization: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl<B> Service<http::Request<B>> for TestDiscoverServer
    where
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::Body>;
        type Error = Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<B>) -> Self::Future {
            if request.uri().path() != "/solti.discover.v1.DiscoverService/Sync" {
                return Box::pin(async move {
                    let mut response = http::Response::new(tonic::body::Body::default());
                    response.headers_mut().insert(
                        tonic::Status::GRPC_STATUS,
                        (tonic::Code::Unimplemented as i32).into(),
                    );
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        tonic::metadata::GRPC_CONTENT_TYPE,
                    );
                    Ok(response)
                });
            }

            struct SyncService {
                expected_authorization: &'static str,
                calls: Arc<AtomicUsize>,
            }

            impl UnaryService<SyncRequest> for SyncService {
                type Response = crate::proto::SyncResponse;
                type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;

                fn call(&mut self, request: tonic::Request<SyncRequest>) -> Self::Future {
                    let expected_authorization = self.expected_authorization;
                    let calls = Arc::clone(&self.calls);
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let authorization = request
                            .metadata()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok());
                        if authorization != Some(expected_authorization) {
                            return Err(tonic::Status::unauthenticated("invalid bearer token"));
                        }
                        if request.get_ref().id != "agent-1" {
                            return Err(tonic::Status::invalid_argument("unexpected agent id"));
                        }

                        Ok(tonic::Response::new(crate::proto::SyncResponse {
                            success: true,
                            reason: String::new(),
                            retry_after_s: 0,
                        }))
                    })
                }
            }

            let service = SyncService {
                expected_authorization: self.expected_authorization,
                calls: Arc::clone(&self.calls),
            };
            Box::pin(async move {
                let codec = tonic_prost::ProstCodec::default();
                let mut grpc = tonic::server::Grpc::new(codec);
                Ok(grpc.unary(service, request).await)
            })
        }
    }

    impl NamedService for TestDiscoverServer {
        const NAME: &'static str = "solti.discover.v1.DiscoverService";
    }

    async fn spawn_test_server(
        expected_authorization: &'static str,
    ) -> (
        std::net::SocketAddr,
        Arc<AtomicUsize>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local discovery listener");
        let address = listener.local_addr().expect("read discovery address");
        let calls = Arc::new(AtomicUsize::new(0));
        let service = TestDiscoverServer {
            expected_authorization,
            calls: Arc::clone(&calls),
        };
        let incoming = TcpIncoming::from(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        (address, calls, shutdown_tx, server)
    }

    async fn stop_test_server(
        shutdown_tx: oneshot::Sender<()>,
        server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    ) {
        shutdown_tx
            .send(())
            .expect("discovery server is still running");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("discovery server shuts down within the bound")
            .expect("discovery server task does not panic")
            .expect("discovery server exits cleanly");
    }

    fn config_builder(endpoint: impl Into<String>) -> crate::DiscoverConfigBuilder {
        DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(endpoint, crate::DiscoveryTransport::Grpc).unwrap(),
            solti_model::AgentCapabilities::default(),
            30_000,
            "test@1",
        )
    }

    fn config(endpoint: impl Into<String>) -> DiscoverConfig {
        config_builder(endpoint).build().expect("config builds")
    }

    #[tokio::test]
    async fn real_grpc_sync_sends_bearer_metadata_and_accepts_success() {
        let (address, calls, shutdown_tx, server) =
            spawn_test_server("Bearer socket-discovery-token").await;
        let config = config_builder(format!("http://{address}"))
            .with_token(solti_model::Token::new("socket-discovery-token").unwrap())
            .allow_insecure_token_transport()
            .build()
            .unwrap();
        let adapter = GrpcAdapter::new(&config).expect("build gRPC adapter");
        let request = SyncRequest {
            id: "agent-1".into(),
            ..SyncRequest::default()
        };

        tokio::time::timeout(Duration::from_secs(2), adapter.sync(request))
            .await
            .expect("gRPC sync finishes within the bound")
            .expect("gRPC sync succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop_test_server(shutdown_tx, server).await;
    }

    #[tokio::test]
    async fn real_grpc_sync_maps_unauthenticated_status() {
        let (address, calls, shutdown_tx, server) =
            spawn_test_server("Bearer expected-token").await;
        let config = config_builder(format!("http://{address}"))
            .with_token(solti_model::Token::new("wrong-token").unwrap())
            .allow_insecure_token_transport()
            .build()
            .unwrap();
        let adapter = GrpcAdapter::new(&config).expect("build gRPC adapter");
        let request = SyncRequest {
            id: "agent-1".into(),
            ..SyncRequest::default()
        };

        let error = tokio::time::timeout(Duration::from_secs(2), adapter.sync(request))
            .await
            .expect("gRPC sync finishes within the bound")
            .expect_err("invalid bearer metadata is rejected");
        let DiscoverError::AuthFailed { reason } = error else {
            panic!("expected AuthFailed, got {error}");
        };
        assert!(reason.contains("Unauthenticated"), "{reason}");
        assert!(reason.contains("invalid bearer token"), "{reason}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop_test_server(shutdown_tx, server).await;
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
            .allow_insecure_token_transport()
            .build()
            .unwrap();

        let result = GrpcAdapter::new(&config);
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[test]
    fn h2c_with_token_is_rejected_by_default() {
        let config = config_builder("http://control.example")
            .with_token(solti_model::Token::new("secret").unwrap())
            .build()
            .unwrap();

        let result = GrpcAdapter::new(&config);
        let Err(DiscoverError::InvalidConfig(message)) = result else {
            panic!("expected h2c token config to be rejected");
        };
        assert!(message.contains("allow_insecure_token_transport()"));
    }

    #[test]
    fn h2c_without_token_remains_allowed() {
        assert!(GrpcAdapter::new(&config("http://control.example")).is_ok());
    }

    #[test]
    fn explicit_opt_in_allows_h2c_with_token() {
        let config = config_builder("http://127.0.0.1:50051")
            .with_token(solti_model::Token::new("development-secret").unwrap())
            .allow_insecure_token_transport()
            .build()
            .unwrap();

        assert!(GrpcAdapter::new(&config).is_ok());
    }

    #[cfg(feature = "tls")]
    #[test]
    fn https_with_token_is_allowed_by_default() {
        let config = config_builder("https://control.example")
            .with_token(solti_model::Token::new("secret").unwrap())
            .build()
            .unwrap();

        assert!(validate_token_transport(&config, true).is_ok());
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
