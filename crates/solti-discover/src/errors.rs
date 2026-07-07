//! # Discovery error types.

use thiserror::Error;

/// Failure modes of the discovery heartbeat.
///
/// Covers config validation, transport failures, response parsing, and control-plane rejections.
/// The sync task maps terminal variants (see [`DiscoverError::is_terminal`]) to `TaskError::Fatal`;
/// all other variants surface as `TaskError::Fail` and the supervisor retries with backoff.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DiscoverError {
    /// Builder validation failed (missing/invalid identity, endpoint, …).
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// The taskvisor `TaskSpec` for the heartbeat task could not be built.
    #[error("failed to build task spec: {0}")]
    SpecBuild(String),

    /// gRPC transport (TCP/TLS/HTTP2) connect failure (feature `grpc`).
    #[cfg(feature = "grpc")]
    #[error("failed to connect to control plane: {0}")]
    GrpcTransport(#[from] tonic::transport::Error),

    /// A non-OK gRPC status from the control plane (feature `grpc`).
    #[cfg(feature = "grpc")]
    #[error("grpc call failed: {0}")]
    GrpcStatus(#[source] Box<tonic::Status>),

    /// HTTP-level request failure - connect/TLS/timeout (feature `http`).
    #[cfg(feature = "http")]
    #[error("http request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    /// A non-2xx HTTP response (feature `http`). `body` is truncated (~1 KiB).
    #[cfg(feature = "http")]
    #[error("http status {code}: {body}")]
    HttpStatus {
        /// HTTP status code.
        code: u16,
        /// Response body, truncated for logging.
        body: String,
    },

    /// The response body could not be deserialized (feature `http`).
    #[cfg(feature = "http")]
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// The control plane returned `success: false`.
    #[error("control plane rejected sync: {reason}")]
    Rejected {
        /// Rejection reason: surfaced **verbatim** from the control plane (untrusted server text).
        reason: String,
        /// Server-advised hold before the next attempt, if any.
        retry_after_s: Option<i32>,
    },

    /// Authentication was rejected (`401`/`403`, or gRPC `Unauthenticated`/`PermissionDenied`).
    #[error("authentication failed: {reason}")]
    AuthFailed {
        /// Why authentication failed (server-provided).
        reason: String,
    },
}

impl DiscoverError {
    /// `true` for errors that indicate a misconfiguration, not a transient failure.
    /// Sync treats these as terminal (Fatal) - the operator must act before the agent can make progress.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            DiscoverError::AuthFailed { .. } | DiscoverError::InvalidConfig(_)
        )
    }
}

#[cfg(feature = "grpc")]
impl From<tonic::Status> for DiscoverError {
    fn from(status: tonic::Status) -> Self {
        DiscoverError::GrpcStatus(Box::new(status))
    }
}
