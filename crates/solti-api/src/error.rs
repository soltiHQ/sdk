//! # API Errors
//!
//! [`ApiError`] is the shared error contract for both transports.
//! HTTP converts it into a Kubernetes-style `Status` resource.
//! gRPC converts it into `tonic::Status`.
//!
//! Write conflicts carry structured [`ApiConflict`] details.
//! Internal diagnostics are logged and hidden from wire clients.

use std::fmt;

use thiserror::Error;

/// One machine-readable cause of an API conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiErrorCause {
    reason: String,
    field: Option<String>,
    message: String,
}

impl ApiErrorCause {
    /// Creates a cause with a reason and readable message.
    pub fn new(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            field: None,
            message: message.into(),
        }
    }

    /// Attaches the related request field.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Returns the machine-readable reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns the related request field.
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// Returns the readable diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Structured optimistic concurrency conflict.
///
/// Each cause describes one failed write precondition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConflict {
    name: String,
    causes: Vec<ApiErrorCause>,
}

impl ApiConflict {
    /// Creates conflict details for one task.
    pub fn new(name: impl Into<String>, causes: Vec<ApiErrorCause>) -> Self {
        Self {
            name: name.into(),
            causes,
        }
    }

    /// Returns the conflicting task name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the failed preconditions.
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

/// Error returned by the handler or a transport boundary.
///
/// | Variant                    | HTTP  | gRPC                |
/// |----------------------------|-------|---------------------|
/// | `InvalidRequest`           | `400` | `InvalidArgument`   |
/// | `Unauthenticated`          | `401` | `Unauthenticated`   |
/// | `AlreadyExists`            | `409` | `AlreadyExists`     |
/// | `Conflict`                 | `409` | `Aborted`           |
/// | `TaskNotFound`, `NotFound` | `404` | `NotFound`          |
/// | `MethodNotAllowed`         | `405` | `Unimplemented`     |
/// | `UnsupportedMediaType`     | `415` | `InvalidArgument`   |
/// | `PayloadTooLarge`          | `413` | `ResourceExhausted` |
/// | `ResourceVersionExpired`   | `410` | `OutOfRange`        |
/// | `Unavailable`              | `503` | `Unavailable`       |
/// | `Internal`                 | `500` | `Internal`          |
///
/// This enum is non-exhaustive.
/// Match it with a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// The request is syntactically or semantically invalid.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The bearer credential is missing, malformed, or rejected.
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    /// A retained task already owns the requested name.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// A write precondition does not match the current task.
    #[error(transparent)]
    Conflict(ApiConflict),

    /// No public task has the requested name.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// No public resource or route matches the request.
    #[error("not found: {0}")]
    NotFound(String),

    /// The resource does not support the requested method.
    #[error("method not allowed: {0}")]
    MethodNotAllowed(String),

    /// The request media type is missing or unsupported.
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),

    /// The request body or message exceeds the configured limit.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// The requested list snapshot or watch position is no longer retained.
    #[error("resource version expired: {0}")]
    ResourceVersionExpired(String),

    /// The service cannot currently accept work.
    #[error("service unavailable: {0}")]
    Unavailable(String),

    /// An unexpected server-side failure occurred.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// Returns the stable variant label.
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
    fn direct_errors_map_to_http_status_codes() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        for (error, expected) in [
            (ApiError::AlreadyExists("x".into()), StatusCode::CONFLICT),
            (
                ApiError::ResourceVersionExpired("old revision".into()),
                StatusCode::GONE,
            ),
            (ApiError::Conflict(conflict()), StatusCode::CONFLICT),
            (
                ApiError::Unavailable("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(error.into_response().status(), expected);
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn direct_errors_map_to_grpc_status_codes() {
        use tonic::Code;

        for (error, expected) in [
            (ApiError::AlreadyExists("x".into()), Code::AlreadyExists),
            (
                ApiError::ResourceVersionExpired("old revision".into()),
                Code::OutOfRange,
            ),
            (ApiError::Unavailable("x".into()), Code::Unavailable),
        ] {
            assert_eq!(tonic::Status::from(error).code(), expected);
        }
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
