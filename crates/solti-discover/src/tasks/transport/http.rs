//! # HTTP discovery
//!
//! ```text
//! SyncRequest
//!      │ POST /api/v1/discovery/sync
//!      ▼
//! reqwest client
//!      ├── non-success status ──► bounded preview ──► DiscoverError
//!      ▼
//! bounded JSON body ──► SyncResponse ──► protocol validation
//! ```
//!
//! One client is reused across task attempts.
//! Redirects are disabled.
//! HTTPS uses platform roots unless custom TLS is configured.

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use solti_model::Token;

use crate::config::DiscoverConfig;
use crate::errors::DiscoverError;
use crate::proto::{SyncRequest, SyncResponse};
use crate::tasks::transport::validate_response;

const DISCOVERY_SYNC_PATH: &str = "/api/v1/discovery/sync";
const MAX_BODY_PREVIEW_BYTES: usize = 1_024;
const MAX_RESPONSE_BODY_BYTES: u64 = 64 * 1_024;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// HTTP/JSON discovery adapter.
pub(in crate::tasks) struct HttpAdapter {
    authorization: Option<HeaderValue>,
    client: reqwest::Client,
    secure: bool,
    url: Url,
}

impl HttpAdapter {
    /// Creates an adapter from the discovery config.
    pub(super) fn new(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        let url = sync_url(&config.control_plane.address)?;
        let secure = url.scheme() == "https";

        #[cfg(feature = "tls")]
        if config.tls.is_some() && !secure {
            return Err(DiscoverError::InvalidConfig(
                "custom TLS requires an https control-plane endpoint".into(),
            ));
        }

        Ok(Self {
            authorization: authorization_header(config.token.as_ref())?,
            client: build_client(config)?,
            secure,
            url,
        })
    }

    /// Sends one discovery request.
    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        let mut request = self.client.post(self.url.clone()).json(&request);
        if let Some(value) = &self.authorization {
            request = request.header(AUTHORIZATION, value.clone());
        }
        let response = request.send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = read_body_preview(response, MAX_BODY_PREVIEW_BYTES).await;
            if matches!(status.as_u16(), 401 | 403) {
                return Err(DiscoverError::AuthFailed {
                    reason: format!("http {}: {body}", status.as_u16()),
                });
            }
            return Err(DiscoverError::HttpStatus {
                code: status.as_u16(),
                body,
            });
        }

        let body = read_body_bounded(response, MAX_RESPONSE_BODY_BYTES).await?;
        let response: SyncResponse = serde_json::from_str(&body).map_err(|error| {
            DiscoverError::InvalidResponse(format!(
                "failed to parse response: {error}, body: {}",
                truncate_body(&body)
            ))
        })?;

        validate_response(response)
    }

    /// Returns whether the endpoint uses HTTPS.
    pub(super) fn is_secure(&self) -> bool {
        self.secure
    }
}

/// Builds the fixed discovery v1 URL.
fn sync_url(endpoint: &str) -> Result<Url, DiscoverError> {
    let mut url = Url::parse(endpoint).map_err(|error| {
        DiscoverError::InvalidConfig(format!("invalid HTTP control-plane endpoint: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DiscoverError::InvalidConfig(
            "HTTP control-plane endpoint must use http or https".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(DiscoverError::InvalidConfig(
            "HTTP control-plane endpoint must not contain a query or fragment".into(),
        ));
    }

    let base_path = url.path().trim_end_matches('/');
    let path = format!("{base_path}{DISCOVERY_SYNC_PATH}");
    url.set_path(&path);
    Ok(url)
}

/// Encodes an optional bearer token.
fn authorization_header(token: Option<&Token>) -> Result<Option<HeaderValue>, DiscoverError> {
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

/// Builds one client for all attempts of the sync task.
///
/// Redirects are disabled to prevent forwarding credentials to another host.
fn build_client(config: &DiscoverConfig) -> Result<reqwest::Client, DiscoverError> {
    #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
        .timeout(Duration::from_millis(config.request_timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT);

    #[cfg(feature = "tls")]
    if let Some(tls) = &config.tls {
        let mut rustls_config = tls
            .clone()
            .into_rustls_config()
            .map_err(|error| DiscoverError::InvalidConfig(format!("load TLS config: {error}")))?;
        rustls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        builder = builder.tls_backend_preconfigured(rustls_config);
    }

    builder.build().map_err(DiscoverError::from)
}

/// Reads a bounded diagnostic body preview.
async fn read_body_preview(mut response: reqwest::Response, max_bytes: usize) -> String {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return format!("<response body exceeds preview cap {max_bytes} bytes>");
    }

    let mut body = Vec::with_capacity(max_bytes.min(1_024));
    let mut truncated = false;

    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => return format!("<failed to read response body: {error}>"),
        };
        let remaining = max_bytes.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() == max_bytes {
            truncated = true;
            break;
        }
    }

    let mut preview = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        preview.push_str("... [truncated]");
    }
    preview
}

/// Reads a successful response body up to the supplied byte limit.
async fn read_body_bounded(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<String, DiscoverError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(DiscoverError::InvalidResponse(format!(
            "response body exceeds cap {max_bytes} bytes"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(DiscoverError::InvalidResponse(format!(
                "response body exceeds cap {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body)
        .map_err(|error| DiscoverError::InvalidResponse(format!("response is not UTF-8: {error}")))
}

/// Truncates diagnostic response text without splitting UTF-8.
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_BODY_PREVIEW_BYTES {
        return body.to_string();
    }

    let mut end = MAX_BODY_PREVIEW_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &body[..end])
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
            crate::ControlPlaneEndpoint::new(endpoint, crate::DiscoveryTransport::Http).unwrap(),
            30_000,
            "test@1",
        )
    }

    fn config(endpoint: impl Into<String>) -> DiscoverConfig {
        config_builder(endpoint).build().expect("config builds")
    }

    async fn one_shot_http_stub(response: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 4_096];
            let _ = socket.read(&mut discard).await;
            socket.write_all(response).await.expect("write response");
        });
        addr
    }

    #[test]
    fn discovery_path_is_fixed_to_v1_and_preserves_base_path() {
        assert_eq!(
            sync_url("https://control.example/base").unwrap().as_str(),
            "https://control.example/base/api/v1/discovery/sync"
        );
    }

    #[test]
    fn endpoint_is_validated_before_task_execution() {
        let result = HttpAdapter::new(&config("ftp://control.example"));
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[test]
    fn https_without_custom_roots_uses_platform_roots() {
        let adapter = HttpAdapter::new(&config("https://control.example")).unwrap();
        assert!(adapter.is_secure());
    }

    #[test]
    fn token_is_validated_before_task_execution() {
        let config = config_builder("http://control.example")
            .with_token(solti_model::Token::new("first\nsecond").unwrap())
            .build()
            .unwrap();

        let result = HttpAdapter::new(&config);
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
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

        let result = HttpAdapter::new(&config);
        assert!(matches!(result, Err(DiscoverError::InvalidConfig(_))));
    }

    #[tokio::test]
    async fn non_success_status_survives_an_oversized_body() {
        let addr =
            one_shot_http_stub(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 70000\r\n\r\ndenied")
                .await;
        let adapter = HttpAdapter::new(&config(format!("http://{addr}"))).unwrap();

        let result = adapter.sync(SyncRequest::default()).await;
        assert!(matches!(result, Err(DiscoverError::AuthFailed { .. })));
    }

    #[tokio::test]
    async fn read_body_bounded_rejects_oversized_chunked_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 1_024];
            let _ = socket.read(&mut discard).await;
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let payload = [b'a'; 4_096];
            loop {
                if socket.write_all(b"1000\r\n").await.is_err()
                    || socket.write_all(&payload).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                    || socket.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let response = build_client(&config(format!("http://{addr}")))
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("stub accepts request");
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            read_body_bounded(response, MAX_RESPONSE_BODY_BYTES),
        )
        .await
        .expect("body cap must stop the stream");

        assert!(matches!(result, Err(DiscoverError::InvalidResponse(_))));
    }

    #[tokio::test]
    async fn client_does_not_follow_redirects() {
        let addr = one_shot_http_stub(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let response = build_client(&config(format!("http://{addr}")))
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("redirect must be returned");

        assert_eq!(response.status().as_u16(), 302);
    }
}
