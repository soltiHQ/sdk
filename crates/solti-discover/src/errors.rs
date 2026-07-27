//! Discovery errors.

use thiserror::Error;

/// Whether a discovery failure may succeed without changing the desired config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retryability {
    /// The supervisor may retry the operation.
    Retryable,
    /// The desired config or protocol interaction must change first.
    Permanent,
}

/// Failure modes of discovery sync.
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
    /// Classifies whether the supervisor may retry this failure.
    pub fn retryability(&self) -> Retryability {
        match self {
            Self::InvalidConfig(_) | Self::SpecBuild(_) | Self::AuthFailed { .. } => {
                Retryability::Permanent
            }
            Self::Rejected { .. } => Retryability::Retryable,
            #[cfg(feature = "http")]
            Self::HttpRequest(_) | Self::InvalidResponse(_) => Retryability::Retryable,
            #[cfg(feature = "http")]
            Self::HttpStatus { code, .. } => {
                if matches!(*code, 408 | 425 | 429) || *code >= 500 {
                    Retryability::Retryable
                } else {
                    Retryability::Permanent
                }
            }
            #[cfg(feature = "grpc")]
            Self::GrpcTransport(_) => Retryability::Retryable,
            #[cfg(feature = "grpc")]
            Self::GrpcStatus(status) => match status.code() {
                tonic::Code::InvalidArgument
                | tonic::Code::NotFound
                | tonic::Code::AlreadyExists
                | tonic::Code::Unauthenticated
                | tonic::Code::PermissionDenied
                | tonic::Code::FailedPrecondition
                | tonic::Code::OutOfRange
                | tonic::Code::Unimplemented => Retryability::Permanent,
                _ => Retryability::Retryable,
            },
        }
    }
}

#[cfg(feature = "grpc")]
impl From<tonic::Status> for DiscoverError {
    fn from(status: tonic::Status) -> Self {
        DiscoverError::GrpcStatus(Box::new(status))
    }
}

#[cfg(all(test, any(feature = "grpc", feature = "http")))]
mod tests {
    use super::*;

    #[cfg(feature = "http")]
    #[test]
    fn http_retryability_distinguishes_client_and_transient_statuses() {
        for code in [400, 404, 409] {
            let error = DiscoverError::HttpStatus {
                code,
                body: String::new(),
            };
            assert_eq!(error.retryability(), Retryability::Permanent);
        }
        for code in [408, 425, 429, 500, 503] {
            let error = DiscoverError::HttpStatus {
                code,
                body: String::new(),
            };
            assert_eq!(error.retryability(), Retryability::Retryable);
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn grpc_retryability_distinguishes_client_and_transient_statuses() {
        for code in [
            tonic::Code::InvalidArgument,
            tonic::Code::NotFound,
            tonic::Code::FailedPrecondition,
            tonic::Code::Unimplemented,
        ] {
            let error = DiscoverError::from(tonic::Status::new(code, "test"));
            assert_eq!(error.retryability(), Retryability::Permanent);
        }
        for code in [
            tonic::Code::DeadlineExceeded,
            tonic::Code::ResourceExhausted,
            tonic::Code::Aborted,
            tonic::Code::Internal,
            tonic::Code::Unavailable,
            tonic::Code::DataLoss,
        ] {
            let error = DiscoverError::from(tonic::Status::new(code, "test"));
            assert_eq!(error.retryability(), Retryability::Retryable);
        }
    }
}
