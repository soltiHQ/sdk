//! # API error types.

use thiserror::Error;

/// Unified error type for both API transports.
///
/// Every handler and conversion failure becomes one of these variants.
/// The transport layers map each variant to a wire response:
/// gRPC via `From<ApiError> for tonic::Status`, HTTP via `axum::response::IntoResponse`
/// (a Kubernetes-style `Status` resource).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// Request was syntactically or semantically invalid (bad field, malformed body, missing required value). → `400` / `InvalidArgument`.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Credential missing, malformed, or rejected. → `401` / `Unauthenticated`.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    /// A retained Task resource already owns the requested name. → `409` / `AlreadyExists`.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// No task matched the requested name/id. → `404` / `NotFound`.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// No public resource or route matched the request. → `404` / `NotFound`.
    #[error("not found: {0}")]
    NotFound(String),

    /// The resource exists, but does not support the requested method. → `405` / `Unimplemented`.
    #[error("method not allowed: {0}")]
    MethodNotAllowed(String),

    /// Request media type is missing or unsupported. → `415` / `InvalidArgument`.
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),

    /// Request body exceeded the configured limit. → `413` / `ResourceExhausted`.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// Service is temporarily unable to accept work. → `503` / `Unavailable`.
    #[error("service unavailable: {0}")]
    Unavailable(String),

    /// Unexpected server-side failure with no more specific mapping. → `500` / `Internal`.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// Short stable diagnostic label for this variant.
    pub fn as_label(&self) -> &'static str {
        match self {
            ApiError::PayloadTooLarge(_) => "PayloadTooLarge",
            ApiError::InvalidRequest(_) => "InvalidRequest",
            ApiError::Unauthenticated(_) => "Unauthenticated",
            ApiError::AlreadyExists(_) => "AlreadyExists",
            ApiError::TaskNotFound(_) => "TaskNotFound",
            ApiError::NotFound(_) => "NotFound",
            ApiError::MethodNotAllowed(_) => "MethodNotAllowed",
            ApiError::UnsupportedMediaType(_) => "UnsupportedMediaType",
            ApiError::Unavailable(_) => "Unavailable",
            ApiError::Internal(_) => "Internal",
        }
    }

    #[cfg(feature = "http")]
    fn http_reason(&self) -> &'static str {
        match self {
            ApiError::InvalidRequest(_) => "BadRequest",
            ApiError::Unauthenticated(_) => "Unauthorized",
            ApiError::AlreadyExists(_) => "AlreadyExists",
            ApiError::TaskNotFound(_) => "NotFound",
            ApiError::NotFound(_) => "NotFound",
            ApiError::MethodNotAllowed(_) => "MethodNotAllowed",
            ApiError::UnsupportedMediaType(_) => "UnsupportedMediaType",
            ApiError::PayloadTooLarge(_) => "RequestEntityTooLarge",
            ApiError::Unavailable(_) => "ServiceUnavailable",
            ApiError::Internal(_) => "InternalError",
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
            ApiError::NotFound(msg) => tonic::Status::not_found(msg),
            ApiError::MethodNotAllowed(msg) => tonic::Status::unimplemented(msg),
            ApiError::UnsupportedMediaType(msg) => tonic::Status::invalid_argument(msg),
            ApiError::Unavailable(msg) => tonic::Status::unavailable(msg),
            ApiError::Internal(msg) => tonic::Status::internal(msg),
        }
    }
}

#[cfg(feature = "http")]
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        use serde::Serialize;

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct StatusResource<'a> {
            api_version: &'static str,
            kind: &'static str,
            metadata: StatusMeta,
            status: &'static str,
            message: &'a str,
            reason: &'static str,
            code: u16,
        }

        #[derive(Serialize)]
        struct StatusMeta {}

        let reason = self.http_reason();
        let (status, message) = match self {
            ApiError::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthenticated(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::AlreadyExists(msg) => (StatusCode::CONFLICT, msg),
            ApiError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::MethodNotAllowed(msg) => (StatusCode::METHOD_NOT_ALLOWED, msg),
            ApiError::UnsupportedMediaType(msg) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, msg),
            ApiError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg),
            ApiError::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = StatusResource {
            api_version: "v1",
            kind: "Status",
            metadata: StatusMeta {},
            status: "Failure",
            message: &message,
            reason,
            code: status.as_u16(),
        };
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
            (ApiError::NotFound("x".into()), "NotFound"),
            (ApiError::MethodNotAllowed("x".into()), "MethodNotAllowed"),
            (
                ApiError::UnsupportedMediaType("x".into()),
                "UnsupportedMediaType",
            ),
            (ApiError::PayloadTooLarge("x".into()), "PayloadTooLarge"),
            (ApiError::Unavailable("x".into()), "Unavailable"),
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

    #[cfg(feature = "http")]
    #[test]
    fn unavailable_maps_to_http_service_unavailable() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let response = ApiError::Unavailable("x".into()).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn unavailable_maps_to_grpc_unavailable() {
        use tonic::Code;

        let status = tonic::Status::from(ApiError::Unavailable("x".into()));
        assert_eq!(status.code(), Code::Unavailable);
    }
}
