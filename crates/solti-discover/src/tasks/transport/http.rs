//! HTTP/JSON discovery transport adapter.

use std::time::Duration;

use solti_model::Token;

use crate::config::DiscoverConfig;
use crate::errors::DiscoverError;
use crate::proto::{SyncRequest, SyncResponse};
use crate::tasks::transport::validate_response;

const MAX_BODY_PREVIEW_BYTES: usize = 1024;
const MAX_RESPONSE_BODY_BYTES: u64 = 64 * 1024;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub(in crate::tasks) struct HttpAdapter {
    client: reqwest::Client,
    endpoint: String,
    api_version: u32,
    token: Option<Token>,
}

impl HttpAdapter {
    pub(super) fn new(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        Ok(Self {
            client: build_client(config)?,
            endpoint: config.control_plane_endpoint.clone(),
            api_version: config.api_version,
            token: config.token.clone(),
        })
    }

    pub(super) async fn sync(&self, request: SyncRequest) -> Result<(), DiscoverError> {
        let url = format!("{}{}", self.endpoint, sync_path(self.api_version));
        let mut request = self.client.post(url).json(&request);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().await?;

        let status = response.status();
        let body = read_body_bounded(response, MAX_RESPONSE_BODY_BYTES).await?;

        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(DiscoverError::AuthFailed {
                    reason: format!("http {}: {}", status.as_u16(), truncate_body(&body)),
                });
            }
            return Err(DiscoverError::HttpStatus {
                code: status.as_u16(),
                body: truncate_body(&body),
            });
        }

        let response: SyncResponse = serde_json::from_str(&body).map_err(|e| {
            DiscoverError::InvalidResponse(format!(
                "failed to parse response: {}, body: {}",
                e,
                truncate_body(&body)
            ))
        })?;

        validate_response(response)
    }
}

/// Build the reqwest client used by the HTTP adapter.
///
/// Redirects are disabled so an operator-configured bearer token is never
/// replayed to a redirect target.
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
            .map_err(|e| DiscoverError::InvalidConfig(format!("tls into_rustls_config: {e}")))?;
        rustls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        builder = builder.tls_backend_preconfigured(rustls_config);
    }

    Ok(builder.build()?)
}

/// HTTP path derived from the discovery API version.
fn sync_path(api_version: u32) -> String {
    format!("/api/v{api_version}/discovery/sync")
}

async fn read_body_bounded(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<String, DiscoverError> {
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(DiscoverError::InvalidResponse(format!(
            "response body {len} bytes exceeds cap {max_bytes}"
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
        .map_err(|e| DiscoverError::InvalidResponse(format!("response body is not UTF-8: {e}")))
}

fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_BODY_PREVIEW_BYTES {
        return body.to_string();
    }

    let mut end = MAX_BODY_PREVIEW_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = body[..end].to_string();
    truncated.push_str("... [truncated]");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DiscoverConfig {
        DiscoverConfig::builder(
            solti_model::AgentId::from("agent-1"),
            "agent-1",
            "http://127.0.0.1:8085",
            "http://127.0.0.1:9000",
            crate::DiscoveryTransport::Http,
            30_000,
            1,
        )
        .build()
        .expect("config builds")
    }

    async fn one_shot_http_stub(response: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket.write_all(response).await.expect("write response");
        });
        addr
    }

    #[test]
    fn sync_path_derives_from_api_version() {
        assert_eq!(sync_path(1), "/api/v1/discovery/sync");
        assert_eq!(sync_path(2), "/api/v2/discovery/sync");
        assert_eq!(sync_path(42), "/api/v42/discovery/sync");
    }

    #[tokio::test]
    async fn read_body_bounded_rejects_oversized_chunked_body_before_stream_ends() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let payload = [b'a'; 4096];
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

        let response = build_client(&config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("stub accepts the request");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            read_body_bounded(response, MAX_RESPONSE_BODY_BYTES),
        )
        .await
        .expect("cap must trip while the stream is still open, not after buffering it");

        match result {
            Err(DiscoverError::InvalidResponse(msg)) => {
                assert!(msg.contains("exceeds cap"), "unexpected message: {msg}");
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_body_bounded_accepts_chunked_body_within_cap() {
        let addr = one_shot_http_stub(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              b\r\nhello world\r\n0\r\n\r\n",
        )
        .await;

        let response = build_client(&config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("stub accepts the request");

        let body = read_body_bounded(response, MAX_RESPONSE_BODY_BYTES)
            .await
            .expect("small chunked body is accepted");
        assert_eq!(body, "hello world");
    }

    #[tokio::test]
    async fn client_does_not_follow_redirects() {
        let addr = one_shot_http_stub(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;

        let response = build_client(&config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("redirect must be returned, not followed");

        assert_eq!(response.status().as_u16(), 302);
    }
}
