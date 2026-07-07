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

    /// No task matched the requested name/id. → `404` / `NotFound`.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// Request body exceeded the configured limit. → `413` / `ResourceExhausted`.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// Unexpected server-side failure with no more specific mapping. → `500` / `Internal`.
    #[error("internal error: {0}")]
    Internal(String),

    /// A failure from the [`solti_core`] layer, mapped variant-by-variant.
    /// `InvalidSpec` → `400`, `AlreadyExists` → `409`, `NotFound` → `404`, everything else → `500`.
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
        CoreError::Supervisor { .. } | CoreError::Mapping(_) | CoreError::Runner(_) => {
            tonic::Status::internal(e.to_string())
        }
        // `CoreError` is `#[non_exhaustive]`: any future variant is conservatively
        // surfaced as `Internal` rather than silently dropped.
        _ => tonic::Status::internal(e.to_string()),
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
        CoreError::Supervisor { .. } | CoreError::Mapping(_) | CoreError::Runner(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
        // `CoreError` is `#[non_exhaustive]`: any future variant is conservatively
        // surfaced as `500 Internal Server Error` rather than silently dropped.
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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

    // `CoreError` is `#[non_exhaustive]`. The compiler therefore no longer forces
    // these mappers to cover every variant. These tests pin the known mappings
    // instead: a maintainer adding a variant should extend the mappers (and this test).
    #[cfg(feature = "http")]
    #[test]
    fn core_to_http_status_maps_every_known_variant() {
        use axum::http::StatusCode;
        use solti_core::CoreError;

        let cases = [
            (
                CoreError::InvalidSpec(solti_model::ModelError::Invalid("x".into())),
                StatusCode::BAD_REQUEST,
            ),
            (CoreError::AlreadyExists("x".into()), StatusCode::CONFLICT),
            (CoreError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (
                CoreError::Supervisor {
                    op: "cancel",
                    source: taskvisor::RuntimeError::TaskAlreadyExists { name: "x".into() }.into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                CoreError::Mapping("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(core_to_http_status(err).0, expected);
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn core_to_status_maps_every_known_variant() {
        use solti_core::CoreError;
        use tonic::Code;

        let cases = [
            (
                CoreError::InvalidSpec(solti_model::ModelError::Invalid("x".into())),
                Code::InvalidArgument,
            ),
            (CoreError::AlreadyExists("x".into()), Code::AlreadyExists),
            (CoreError::NotFound("x".into()), Code::NotFound),
            (
                CoreError::Supervisor {
                    op: "cancel",
                    source: taskvisor::RuntimeError::TaskAlreadyExists { name: "x".into() }.into(),
                },
                Code::Internal,
            ),
            (CoreError::Mapping("x".into()), Code::Internal),
        ];
        for (err, expected) in cases {
            assert_eq!(core_to_status(err).code(), expected);
        }
    }
}
