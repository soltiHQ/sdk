//! # API error types.

use std::fmt;

use thiserror::Error;

/// One machine-readable cause attached to an API error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiErrorCause {
    reason: String,
    field: Option<String>,
    message: String,
}

impl ApiErrorCause {
    /// Create a cause with a stable reason and diagnostic message.
    pub fn new(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            field: None,
            message: message.into(),
        }
    }

    /// Attach the request field responsible for this cause.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Stable cause reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Related request field, when known.
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// Human-readable diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Structured optimistic-concurrency conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConflict {
    name: String,
    causes: Vec<ApiErrorCause>,
}

impl ApiConflict {
    /// Create conflict details for one Task resource.
    pub fn new(name: impl Into<String>, causes: Vec<ApiErrorCause>) -> Self {
        Self {
            name: name.into(),
            causes,
        }
    }

    /// Conflicting resource name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Failed write preconditions.
    pub fn causes(&self) -> &[ApiErrorCause] {
        &self.causes
    }
}

impl fmt::Display for ApiConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "write precondition failed for Task `{}`",
            self.name
        )?;
        for (index, cause) in self.causes.iter().enumerate() {
            if index == 0 {
                formatter.write_str(": ")?;
            } else {
                formatter.write_str("; ")?;
            }
            formatter.write_str(cause.message())?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiConflict {}

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

    /// A write precondition did not match the current resource. → `409` / `Aborted`.
    #[error(transparent)]
    Conflict(ApiConflict),

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

    /// The requested collection snapshot or watch position is no longer retained. → `410` / `OutOfRange`.
    #[error("resource version expired: {0}")]
    ResourceVersionExpired(String),

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
            ApiError::Conflict(_) => "Conflict",
            ApiError::TaskNotFound(_) => "TaskNotFound",
            ApiError::NotFound(_) => "NotFound",
            ApiError::MethodNotAllowed(_) => "MethodNotAllowed",
            ApiError::UnsupportedMediaType(_) => "UnsupportedMediaType",
            ApiError::ResourceVersionExpired(_) => "ResourceVersionExpired",
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
            ApiError::Conflict(_) => "Conflict",
            ApiError::TaskNotFound(_) => "NotFound",
            ApiError::NotFound(_) => "NotFound",
            ApiError::MethodNotAllowed(_) => "MethodNotAllowed",
            ApiError::UnsupportedMediaType(_) => "UnsupportedMediaType",
            ApiError::PayloadTooLarge(_) => "RequestEntityTooLarge",
            ApiError::ResourceVersionExpired(_) => "Expired",
            ApiError::Unavailable(_) => "ServiceUnavailable",
            ApiError::Internal(_) => "InternalError",
        }
    }

    #[cfg(feature = "http")]
    pub(crate) fn into_http_status(self) -> (axum::http::StatusCode, HttpStatusResource) {
        use axum::http::StatusCode;

        let reason = self.http_reason();
        let (status, message, details) = match self {
            ApiError::InvalidRequest(msg) => (StatusCode::BAD_REQUEST, msg, None),
            ApiError::Unauthenticated(msg) => (StatusCode::UNAUTHORIZED, msg, None),
            ApiError::AlreadyExists(msg) => (StatusCode::CONFLICT, msg, None),
            ApiError::Conflict(conflict) => {
                let details = HttpStatusDetails {
                    name: conflict.name().to_owned(),
                    group: "solti.io",
                    kind: "Task",
                    causes: conflict
                        .causes()
                        .iter()
                        .map(|cause| HttpStatusCause {
                            reason: cause.reason().to_owned(),
                            field: cause.field().map(http_field_path),
                            message: cause.message().to_owned(),
                        })
                        .collect(),
                };
                (StatusCode::CONFLICT, conflict.to_string(), Some(details))
            }
            ApiError::TaskNotFound(msg) => (StatusCode::NOT_FOUND, msg, None),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg, None),
            ApiError::MethodNotAllowed(msg) => (StatusCode::METHOD_NOT_ALLOWED, msg, None),
            ApiError::UnsupportedMediaType(msg) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, msg, None),
            ApiError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg, None),
            ApiError::ResourceVersionExpired(msg) => (StatusCode::GONE, msg, None),
            ApiError::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg, None),
            ApiError::Internal(msg) => {
                tracing::error!(error = %msg, "API request failed internally");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_owned(),
                    None,
                )
            }
        };

        let body = HttpStatusResource {
            api_version: "v1",
            kind: "Status",
            metadata: HttpStatusMeta {},
            status: "Failure",
            message,
            reason,
            details,
            code: status.as_u16(),
        };
        (status, body)
    }
}

#[cfg(feature = "http")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpStatusResource {
    api_version: &'static str,
    kind: &'static str,
    metadata: HttpStatusMeta,
    status: &'static str,
    message: String,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<HttpStatusDetails>,
    code: u16,
}

#[cfg(feature = "http")]
#[derive(serde::Serialize)]
struct HttpStatusMeta {}

#[cfg(feature = "http")]
#[derive(serde::Serialize)]
struct HttpStatusDetails {
    name: String,
    group: &'static str,
    kind: &'static str,
    causes: Vec<HttpStatusCause>,
}

#[cfg(feature = "http")]
#[derive(serde::Serialize)]
struct HttpStatusCause {
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    message: String,
}

#[cfg(feature = "grpc")]
impl From<ApiError> for tonic::Status {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::PayloadTooLarge(msg) => tonic::Status::resource_exhausted(msg),
            ApiError::InvalidRequest(msg) => tonic::Status::invalid_argument(msg),
            ApiError::Unauthenticated(msg) => tonic::Status::unauthenticated(msg),
            ApiError::AlreadyExists(msg) => tonic::Status::already_exists(msg),
            ApiError::Conflict(conflict) => {
                use prost::Message as _;

                let details = crate::proto_api::WriteConflictDetails {
                    name: conflict.name().to_owned(),
                    causes: conflict
                        .causes()
                        .iter()
                        .map(|cause| crate::proto_api::StatusCause {
                            reason: cause.reason().to_owned(),
                            field: cause.field().map(str::to_owned),
                            message: cause.message().to_owned(),
                        })
                        .collect(),
                };
                tonic::Status::with_details(
                    tonic::Code::Aborted,
                    conflict.to_string(),
                    details.encode_to_vec().into(),
                )
            }
            ApiError::TaskNotFound(msg) => tonic::Status::not_found(msg),
            ApiError::NotFound(msg) => tonic::Status::not_found(msg),
            ApiError::MethodNotAllowed(msg) => tonic::Status::unimplemented(msg),
            ApiError::UnsupportedMediaType(msg) => tonic::Status::invalid_argument(msg),
            ApiError::ResourceVersionExpired(msg) => tonic::Status::out_of_range(msg),
            ApiError::Unavailable(msg) => tonic::Status::unavailable(msg),
            ApiError::Internal(msg) => {
                tracing::error!(error = %msg, "API request failed internally");
                tonic::Status::internal("internal server error")
            }
        }
    }
}

#[cfg(feature = "http")]
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = self.into_http_status();
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(feature = "http")]
fn http_field_path(field: &str) -> String {
    match field {
        "preconditions.uid" => "uid".to_owned(),
        "preconditions.resourceVersion" => "resourceVersion".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict() -> ApiConflict {
        ApiConflict::new(
            "task-1",
            vec![
                ApiErrorCause::new("ResourceVersionMismatch", "expected `1`, current `2`")
                    .with_field("preconditions.resourceVersion"),
            ],
        )
    }

    #[test]
    fn as_label_covers_all_direct_variants() {
        let cases = [
            (ApiError::InvalidRequest("x".into()), "InvalidRequest"),
            (ApiError::Unauthenticated("x".into()), "Unauthenticated"),
            (ApiError::AlreadyExists("x".into()), "AlreadyExists"),
            (ApiError::Conflict(conflict()), "Conflict"),
            (ApiError::TaskNotFound("x".into()), "TaskNotFound"),
            (ApiError::NotFound("x".into()), "NotFound"),
            (ApiError::MethodNotAllowed("x".into()), "MethodNotAllowed"),
            (
                ApiError::UnsupportedMediaType("x".into()),
                "UnsupportedMediaType",
            ),
            (ApiError::PayloadTooLarge("x".into()), "PayloadTooLarge"),
            (
                ApiError::ResourceVersionExpired("x".into()),
                "ResourceVersionExpired",
            ),
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
    fn expired_resource_version_maps_to_http_gone() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let response = ApiError::ResourceVersionExpired("old revision".into()).into_response();
        assert_eq!(response.status(), StatusCode::GONE);
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn expired_resource_version_maps_to_grpc_out_of_range() {
        use tonic::Code;

        let status = tonic::Status::from(ApiError::ResourceVersionExpired("old revision".into()));
        assert_eq!(status.code(), Code::OutOfRange);
    }

    #[cfg(feature = "http")]
    #[test]
    fn conflict_maps_to_http_conflict() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let response = ApiError::Conflict(conflict()).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn conflict_maps_to_grpc_aborted() {
        use prost::Message as _;
        use tonic::Code;

        let status = tonic::Status::from(ApiError::Conflict(conflict()));
        assert_eq!(status.code(), Code::Aborted);
        let details = crate::proto_api::WriteConflictDetails::decode(status.details()).unwrap();
        assert_eq!(details.name, "task-1");
        assert_eq!(details.causes.len(), 1);
        assert_eq!(details.causes[0].reason, "ResourceVersionMismatch");
        assert_eq!(
            details.causes[0].field.as_deref(),
            Some("preconditions.resourceVersion")
        );
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn conflict_http_status_contains_structured_causes() {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;

        let response = ApiError::Conflict(conflict()).into_response();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["reason"], "Conflict");
        assert_eq!(value["details"]["name"], "task-1");
        assert_eq!(value["details"]["group"], "solti.io");
        assert_eq!(value["details"]["kind"], "Task");
        assert_eq!(
            value["details"]["causes"][0]["reason"],
            "ResourceVersionMismatch"
        );
        assert_eq!(value["details"]["causes"][0]["field"], "resourceVersion");
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

    #[cfg(feature = "grpc")]
    #[test]
    fn grpc_internal_error_hides_diagnostic_message() {
        let status = tonic::Status::from(ApiError::Internal("secret diagnostic".into()));
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "internal server error");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn http_internal_error_hides_diagnostic_message() {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;

        let response = ApiError::Internal("secret diagnostic".into()).into_response();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["message"], "internal server error");
        assert!(!String::from_utf8_lossy(&body).contains("secret diagnostic"));
    }
}
