//! # Discovery error types.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum DiscoverError {
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("failed to build task spec: {0}")]
    SpecBuild(String),

    #[cfg(feature = "grpc")]
    #[error("failed to connect to control plane: {0}")]
    GrpcTransport(#[from] tonic::transport::Error),

    #[cfg(feature = "grpc")]
    #[error("grpc call failed: {0}")]
    GrpcStatus(#[source] Box<tonic::Status>),

    #[cfg(feature = "http")]
    #[error("http request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    /// Non-2xx HTTP status. `body` is truncated to ~1 KiB.
    #[cfg(feature = "http")]
    #[error("http status {code}: {body}")]
    HttpStatus { code: u16, body: String },

    #[cfg(feature = "http")]
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("control plane rejected sync: {reason}")]
    Rejected {
        reason: String,
        /// Server-suggested minimum delay before next attempt (seconds).
        /// `None` when the server did not advise a specific delay.
        retry_after_s: Option<i32>,
    },
}

#[cfg(feature = "grpc")]
impl From<tonic::Status> for DiscoverError {
    fn from(status: tonic::Status) -> Self {
        DiscoverError::GrpcStatus(Box::new(status))
    }
}
