//! # API error types.

use thiserror::Error;

/// Unified error type for both API transports.
///
/// Every handler and conversion failure becomes one of these variants.
/// The transport layers map each variant to a wire response:
/// gRPC via `From<ApiError> for tonic::Status`, HTTP via `axum::response::IntoResponse`
/// (JSON body `{ "error": <label>, "message": <detail> }`).
/// The stable `error` label comes from [`ApiError::as_label`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// Request was syntactically or semantically invalid (bad field, malformed body, missing required value). → `400` / `InvalidArgument`.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Credential missing, malformed, or rejected. → `401` / `Unauthenticated`.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    /// A live task already owns the requested identity. → `409` / `AlreadyExists`.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// No task matched the requested name/id. → `404` / `NotFound`.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// Request body exceeded the configured limit. → `413` / `ResourceExhausted`.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// Unexpected server-side failure with no more specific mapping. → `500` / `Internal`.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// Short stable label for this variant, surfaced in HTTP error bodies and logs.
    pub fn as_label(&self) -> &'static str {
        match self {
            ApiError::PayloadTooLarge(_) => "PayloadTooLarge",
            ApiError::InvalidRequest(_) => "InvalidRequest",
            ApiError::Unauthenticated(_) => "Unauthenticated",
            ApiError::AlreadyExists(_) => "AlreadyExists",
            ApiError::TaskNotFound(_) => "TaskNotFound",
            ApiError::Internal(_) => "Internal",
        }
    }
}

#[cfg(feature = "grpc")]
impl From<ApiError> for tonic::Status {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::PayloadTooLarge(msg) => tonic::Status::resource_exhausted(msg),
            ApiError::InvalidRequest(msg) => tonic::Status::invalid_argument(msg),
            ApiError::Unauthenticated(msg) => tonic::Status::unauthenticated(msg),
            ApiError::AlreadyExists(msg) => tonic::Status::already_exists(msg),
            ApiError::TaskNotFound(msg) => tonic::Status::not_found(msg),
            ApiError::Internal(msg) => tonic::Status::internal(msg),
        }
    }
}

#[cfg(feature = "http")]
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;

        let label = self.as_label();
        let (status, message) = match self {
            ApiError::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthenticated(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::AlreadyExists(msg) => (StatusCode::CONFLICT, msg),
            ApiError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = serde_json::json!({ "error": label, "message": message });
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_label_covers_all_direct_variants() {
        let cases = [
            (ApiError::InvalidRequest("x".into()), "InvalidRequest"),
            (ApiError::Unauthenticated("x".into()), "Unauthenticated"),
            (ApiError::AlreadyExists("x".into()), "AlreadyExists"),
            (ApiError::TaskNotFound("x".into()), "TaskNotFound"),
            (ApiError::PayloadTooLarge("x".into()), "PayloadTooLarge"),
            (ApiError::Internal("x".into()), "Internal"),
        ];

        for (error, expected) in cases {
            assert_eq!(error.as_label(), expected);
        }
    }

    #[cfg(feature = "http")]
    #[test]
    fn already_exists_maps_to_http_conflict() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let response = ApiError::AlreadyExists("x".into()).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn already_exists_maps_to_grpc_already_exists() {
        use tonic::Code;

        let status = tonic::Status::from(ApiError::AlreadyExists("x".into()));
        assert_eq!(status.code(), Code::AlreadyExists);
    }
}
