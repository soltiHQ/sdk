//! # Discovery errors
//!
//! [`DiscoverError`] describes configuration, transport, and protocol failures.
//! [`Retryability`] tells the embedded task how to return the failure.

use thiserror::Error;

/// Whether a discovery failure can be retried with the same desired config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retryability {
    /// The supervisor can retry the operation.
    Retryable,
    /// The config or protocol interaction must change first.
    Permanent,
}

/// Error from configuration or discovery sync.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DiscoverError {
    /// Configuration is incomplete or cannot be used.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// The embedded task manifest could not be built.
    #[error("failed to build task spec: {0}")]
    SpecBuild(String),

    /// The gRPC client could not connect.
    #[cfg(feature = "grpc")]
    #[error("failed to connect to control plane: {0}")]
    GrpcTransport(#[from] tonic::transport::Error),

    /// The control plane returned a non-OK gRPC status.
    #[cfg(feature = "grpc")]
    #[error("grpc call failed: {0}")]
    GrpcStatus(#[source] Box<tonic::Status>),

    /// The HTTP connection, request, or response body failed.
    #[cfg(feature = "http")]
    #[error("http request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    /// The control plane returned a non-success HTTP status.
    ///
    /// `body` contains a bounded prefix or a diagnostic marker.
    #[cfg(feature = "http")]
    #[error("http status {code}: {body}")]
    HttpStatus {
        /// HTTP status code.
        code: u16,
        /// Response body preview or read error marker.
        body: String,
    },

    /// The HTTP response body is too large, invalid UTF-8, or invalid JSON.
    #[cfg(feature = "http")]
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Discovery protocol v1 returned `success = false`.
    #[error("control plane rejected sync: {reason}")]
    Rejected {
        /// Untrusted reason returned by the control plane.
        reason: String,
        /// Server-advised hold before the next attempt.
        retry_after_s: Option<i32>,
    },

    /// HTTP or gRPC authentication was rejected.
    #[error("authentication failed: {reason}")]
    AuthFailed {
        /// Untrusted reason returned by the control plane.
        reason: String,
    },
}

impl DiscoverError {
    /// Classifies whether the supervisor may retry this failure.
    ///
    /// HTTP `408`, `425`, `429`, and `5xx` statuses are retryable.
    /// Permanent gRPC client statuses are not retryable.
    /// Other gRPC statuses are retryable.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_retryability_matches_recovery_requirements() {
        for error in [
            DiscoverError::InvalidConfig("invalid".into()),
            DiscoverError::SpecBuild("invalid".into()),
            DiscoverError::AuthFailed {
                reason: "denied".into(),
            },
        ] {
            assert_eq!(error.retryability(), Retryability::Permanent);
        }

        let rejected = DiscoverError::Rejected {
            reason: "overloaded".into(),
            retry_after_s: Some(60),
        };
        assert_eq!(rejected.retryability(), Retryability::Retryable);
    }

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
