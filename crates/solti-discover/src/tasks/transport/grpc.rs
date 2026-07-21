//! gRPC discovery transport adapter.

use std::time::Duration;

use solti_model::Token;
use tonic::transport::Channel;

use crate::config::DiscoverConfig;
use crate::errors::DiscoverError;
use crate::proto::SyncRequest;
use crate::proto::discover_service_client::DiscoverServiceClient;
use crate::tasks::transport::validate_response;

pub(in crate::tasks) struct GrpcAdapter {
    endpoint: String,
    connect_timeout: Duration,
    request_timeout: Duration,
    token: Option<Token>,
    #[cfg(feature = "tls")]
    tls: Option<solti_tls::LoadedClientTlsConfig>,
    client: tokio::sync::OnceCell<DiscoverServiceClient<Channel>>,
}

impl GrpcAdapter {
    pub(super) fn new(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        Ok(Self {
            endpoint: config.control_plane_endpoint.clone(),
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            token: config.token.clone(),
            #[cfg(feature = "tls")]
            tls: config
                .tls
                .clone()
                .map(solti_tls::ClientTlsConfig::load)
                .transpose()
                .map_err(|error| {
                    DiscoverError::InvalidConfig(format!("load TLS material: {error}"))
                })?,
            client: tokio::sync::OnceCell::new(),
        })
    }

    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        let client = self
            .client
            .get_or_try_init(|| async {
                #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
                let mut endpoint = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
                    .map_err(|e| {
                        DiscoverError::InvalidConfig(format!("invalid control_plane_endpoint: {e}"))
                    })?
                    .connect_timeout(self.connect_timeout)
                    .timeout(self.request_timeout);

                #[cfg(feature = "tls")]
                if let Some(tls) = &self.tls {
                    endpoint = endpoint
                        .tls_config(build_tonic_client_tls(tls))
                        .map_err(|e| DiscoverError::InvalidConfig(format!("tls_config: {e}")))?;
                }

                let channel = endpoint.connect().await?;
                Ok::<_, DiscoverError>(DiscoverServiceClient::new(channel))
            })
            .await?;

        let mut client = client.clone();
        let mut request = tonic::Request::new(request);
        if let Some(token) = &self.token {
            let value: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
                format!("Bearer {}", token.expose()).parse().map_err(|_| {
                    DiscoverError::InvalidConfig(
                        "token contains characters invalid for an Authorization header".into(),
                    )
                })?;
            request.metadata_mut().insert("authorization", value);
        }

        match client.sync(request).await {
            Ok(response) => validate_response(response.into_inner()),
            Err(status) => match status.code() {
                tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
                    Err(DiscoverError::AuthFailed {
                        reason: format!("grpc {:?}: {}", status.code(), status.message()),
                    })
                }
                _ => Err(DiscoverError::from(status)),
            },
        }
    }
}

/// Convert the shared TLS config into tonic's PEM-blob TLS config.
#[cfg(feature = "tls")]
fn build_tonic_client_tls(
    cfg: &solti_tls::LoadedClientTlsConfig,
) -> tonic::transport::ClientTlsConfig {
    use tonic::transport::{Certificate, ClientTlsConfig as TonicTls, Identity};

    let mut tls = TonicTls::new().ca_certificate(Certificate::from_pem(cfg.server_roots_pem()));

    if let Some(identity) = cfg.identity() {
        tls = tls.identity(Identity::from_pem(
            identity.certificate_chain_pem(),
            identity.expose_private_key_pem(),
        ));
    }

    tls
}
