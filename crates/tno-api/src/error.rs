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

#[cfg(feature = "http")]
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;

        let (status, message) = match self {
            ApiError::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };

        let body = serde_json::json!({
            "error": message
        });

        (status, axum::Json(body)).into_response()
    }
}
