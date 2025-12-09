use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("core error: {0}")]
    Core(#[from] tno_core::CoreError),
}

#[cfg(feature = "grpc")]
impl From<ApiError> for tonic::Status {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::InvalidRequest(msg) => tonic::Status::invalid_argument(msg),
            ApiError::TaskNotFound(msg) => tonic::Status::not_found(msg),
            ApiError::Internal(msg) => tonic::Status::internal(format!("internal error: {}", msg)),
            ApiError::Core(e) => tonic::Status::internal(format!("core error: {}", e)),
        }
    }
}
