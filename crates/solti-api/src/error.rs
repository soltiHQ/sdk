//! # API error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("core error: {0}")]
    Core(#[from] solti_core::CoreError),
}

impl ApiError {
    /// Short stable label for this variant, surfaced in HTTP error bodies and logs.
    ///
    /// `Core` is flattened to the same two buckets used by the wire mappings:
    /// `InvalidSpec` presents as `InvalidRequest`, anything else as `Internal`.
    pub fn as_label(&self) -> &'static str {
        match self {
            ApiError::Core(solti_core::CoreError::InvalidSpec(_)) => "InvalidRequest",
            ApiError::Core(solti_core::CoreError::AlreadyExists(_)) => "AlreadyExists",
            ApiError::Core(solti_core::CoreError::NotFound(_)) => "TaskNotFound",
            ApiError::PayloadTooLarge(_) => "PayloadTooLarge",
            ApiError::InvalidRequest(_) => "InvalidRequest",
            ApiError::Unauthenticated(_) => "Unauthenticated",
            ApiError::TaskNotFound(_) => "TaskNotFound",
            ApiError::Internal(_) => "Internal",
            ApiError::Core(_) => "Internal",
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
            ApiError::TaskNotFound(msg) => tonic::Status::not_found(msg),
            ApiError::Internal(msg) => tonic::Status::internal(msg),
            ApiError::Core(e) => core_to_status(e),
        }
    }
}

#[cfg(feature = "grpc")]
fn core_to_status(e: solti_core::CoreError) -> tonic::Status {
    use solti_core::CoreError;
    match e {
        CoreError::InvalidSpec(inner) => tonic::Status::invalid_argument(inner.to_string()),
        CoreError::AlreadyExists(msg) => tonic::Status::already_exists(msg),
        CoreError::NotFound(msg) => tonic::Status::not_found(msg),
        CoreError::Supervisor(_) | CoreError::Mapping(_) | CoreError::Runner(_) => {
            tonic::Status::internal(e.to_string())
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
            ApiError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::Core(e) => core_to_http_status(e),
        };

        let body = serde_json::json!({ "error": label, "message": message });
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(feature = "http")]
fn core_to_http_status(e: solti_core::CoreError) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;
    use solti_core::CoreError;
    match e {
        CoreError::InvalidSpec(inner) => (StatusCode::BAD_REQUEST, inner.to_string()),
        CoreError::AlreadyExists(msg) => (StatusCode::CONFLICT, msg),
        CoreError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
        CoreError::Supervisor(_) | CoreError::Mapping(_) | CoreError::Runner(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_label_covers_all_direct_variants() {
        assert_eq!(
            ApiError::InvalidRequest("x".into()).as_label(),
            "InvalidRequest"
        );
        assert_eq!(
            ApiError::TaskNotFound("x".into()).as_label(),
            "TaskNotFound"
        );
        assert_eq!(ApiError::Internal("x".into()).as_label(), "Internal");
    }

    #[test]
    fn as_label_flattens_core_invalid_spec_to_invalid_request() {
        let inner = solti_model::ModelError::Invalid("bad".into());
        let e = ApiError::Core(solti_core::CoreError::InvalidSpec(inner));
        assert_eq!(e.as_label(), "InvalidRequest");
    }

    #[test]
    fn as_label_maps_core_already_exists_and_not_found() {
        let dup = ApiError::Core(solti_core::CoreError::AlreadyExists("t".into()));
        assert_eq!(dup.as_label(), "AlreadyExists");

        let missing = ApiError::Core(solti_core::CoreError::NotFound("t".into()));
        assert_eq!(missing.as_label(), "TaskNotFound");
    }

    #[cfg(feature = "http")]
    #[test]
    fn core_already_exists_is_conflict_and_not_found_is_404() {
        use axum::http::StatusCode;
        let (status, _) = core_to_http_status(solti_core::CoreError::AlreadyExists("t".into()));
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = core_to_http_status(solti_core::CoreError::NotFound("t".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
