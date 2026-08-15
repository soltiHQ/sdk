//! # HTTP Transport
//!
//! Axum router for the model-owned CRD JSON representation.
//! Every operation delegates to [`ApiHandler`].
//!
//! The current API root is `/apis/solti.io/v1`.
//!
//! ## Routes
//!
//! | Method   | Path                                          | Operation   |
//! |----------|-----------------------------------------------|-------------|
//! | `POST`   | `/apis/solti.io/v1/tasks`                     | Create      |
//! | `PUT`    | `/apis/solti.io/v1/tasks/{name}`              | Apply       |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}`              | Get         |
//! | `GET`    | `/apis/solti.io/v1/tasks`                     | List        |
//! | `GET`    | `/apis/solti.io/v1/tasks?watch=true`          | Watch       |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}/runs`         | Run history |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}/logs`         | Live output |
//! | `DELETE` | `/apis/solti.io/v1/tasks/{name}`              | Delete      |
//!
//! ## Wire Shapes
//!
//! Create and apply accept `TaskManifest` JSON.
//! Resource reads return `Task` JSON.
//! Lists return a Kubernetes-style `TaskList`.
//! Errors return a Kubernetes-style `Status`.
//!
//! Watches emit newline-delimited JSON documents.
//! Live output uses Server-Sent Events.
//! Both streams remain open until their source ends or fails.
//!
//! Every request body is limited to [`crate::MAX_REQUEST_BYTES`].
//! Task and TaskRun list bodies have separate 4 MiB response limits.

use std::{
    convert::Infallible,
    io::{self, Write},
    num::NonZeroU32,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use aide::{
    NoApi,
    axum::{
        ApiRouter,
        routing::{get_with, post_with},
    },
    generate::GenContext,
    openapi::{
        Info, MediaType, OpenApi, ReferenceOr, Response as ApiResponse, SchemaObject,
        SecurityScheme, Tag,
    },
    operation::{OperationInput, OperationOutput},
    transform::TransformOperation,
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, RawQuery, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    handler::HandlerWithoutStateExt,
    http::{HeaderValue, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{
        IntoResponse, NoContent, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use schemars::{JsonSchema, Schema, generate::SchemaSettings};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, ContainerSpec, ExtensionWorkload, LabelSelector, ObjectMeta,
    OutputEvent, RestartPolicy, Slot, SubprocessSpec, TASK_API_VERSION, Task, TaskFilter, TaskId,
    TaskManifest, TaskManifestMeta, TaskPhase, TaskQuery, TaskRun, TaskRunPage, TaskRunQuery,
    TaskStatus, TaskWatchEvent, Timeout, Token, TypeMeta, Uid, WORKLOAD_API_VERSION, WasmSpec,
    WritePreconditions,
};
use tokio_stream::{Stream, StreamExt};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::debug;

use crate::{
    API_VERSION, API_VERSION_NAME, ApiAuthenticatorHandle, ApiAuthorizerHandle, ApiIdentity,
    AuthenticationRequest, AuthorizationRequest, GRPC_API_PACKAGE, HTTP_API_ROOT,
    MAX_REQUEST_BYTES, TaskOperation, TaskTarget, Transport,
    auth::{StaticBearerAuthenticator, bearer_value},
    error::{ApiError, HttpStatusResource},
    handler::{ApiHandler, TaskWatchEventStream},
    metrics::{ApiMetricsHandle, StreamingResponse, http_metrics_middleware, noop_api_metrics},
    validate::{parse_list_limit, parse_task_id, parse_task_run_limit, validate_slot},
    visibility::{manifest_is_visible, task_is_visible},
};

const HTTP_BEARER_SCHEME: &str = "soltiTaskBearer";

/// Task manifest extractor with the public HTTP schema.
struct TaskManifestJson(TaskManifest);

impl<S> FromRequest<S> for TaskManifestJson
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = axum::Json::<TaskManifest>::from_request(req, state)
            .await
            .map_err(map_json_rejection)?;
        Ok(Self(value))
    }
}

impl OperationInput for TaskManifestJson {
    fn operation_input(context: &mut GenContext, operation: &mut aide::openapi::Operation) {
        <Json<HttpTaskManifestSchema> as OperationInput>::operation_input(context, operation);
    }
}

/// Query extractor that maps axum rejections into [`ApiError`].
pub(crate) struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(value) = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(map_query_rejection)?;
        Ok(ApiQuery(value))
    }
}

impl<T> OperationInput for ApiQuery<T>
where
    T: JsonSchema,
{
    fn operation_input(context: &mut GenContext, operation: &mut aide::openapi::Operation) {
        <Query<T> as OperationInput>::operation_input(context, operation);
    }
}

/// Path extractor that maps axum rejections into [`ApiError`].
pub(crate) struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(map_path_rejection)?;
        Ok(ApiPath(value))
    }
}

impl<T> OperationInput for ApiPath<T>
where
    T: JsonSchema,
{
    fn operation_input(context: &mut GenContext, operation: &mut aide::openapi::Operation) {
        <Path<T> as OperationInput>::operation_input(context, operation);
    }
}

fn map_path_rejection(rejection: PathRejection) -> ApiError {
    ApiError::InvalidRequest(rejection.body_text())
}

fn map_query_rejection(rejection: QueryRejection) -> ApiError {
    ApiError::InvalidRequest(rejection.body_text())
}

fn map_json_rejection(rej: JsonRejection) -> ApiError {
    if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::PayloadTooLarge(format!(
            "request body exceeds the maximum of {} bytes",
            MAX_REQUEST_BYTES
        ));
    }
    if rej.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return ApiError::UnsupportedMediaType(rej.body_text());
    }

    let msg = rej.body_text();
    let trimmed = msg
        .strip_prefix("Failed to deserialize the JSON body into the target type: ")
        .or_else(|| msg.strip_prefix("Failed to parse the request body as JSON: "))
        .unwrap_or(&msg)
        .to_string();
    ApiError::InvalidRequest(trimmed)
}

async fn map_413_envelope(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::PayloadTooLarge(format!(
            "request body exceeds the maximum of {} bytes",
            MAX_REQUEST_BYTES
        ))
        .into_response();
    }
    resp
}

async fn route_not_found(req: Request) -> Response {
    ApiError::NotFound(format!("no API route for `{}`", req.uri().path())).into_response()
}

async fn method_not_allowed(req: Request) -> Response {
    ApiError::MethodNotAllowed(format!(
        "method {} is not allowed for `{}`",
        req.method(),
        req.uri().path()
    ))
    .into_response()
}

/// Standalone runtime router and its generated OpenAPI contract.
///
/// The document describes the exact routes installed on [`router`](Self::router).
/// It is generated in memory and is not written to the source tree.
pub struct HttpApiParts {
    /// Configured task API router.
    pub router: Router,

    /// OpenAPI 3.1 document for the configured router.
    pub openapi: OpenApi,
}

/// Builder for the axum task API.
///
/// Authentication, authorization, and metrics are optional.
/// [`build`](Self::build) returns a standalone router and OpenAPI document.
/// [`mount`](Self::mount) adds the documented task subtree to an application router.
/// [`router`](Self::router) returns only the router.
///
/// ## Example
///
/// ```rust,no_run
/// use std::sync::Arc;
///
/// use solti_api::{ApiHandler, HttpApi};
/// use solti_model::Token;
///
/// fn build<H: ApiHandler>(
///     handler: Arc<H>,
///     token: Token,
/// ) -> solti_api::axum::Router {
///     HttpApi::new(handler)
///         .with_auth(token)
///         .router()
/// }
/// ```
///
/// ## See Also
///
/// - [`ApiHandler`] defines the backend operations.
/// - [`ApiError`] defines the HTTP `Status` mapping.
pub struct HttpApi<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
    authenticator: Option<ApiAuthenticatorHandle>,
    authorizer: Option<ApiAuthorizerHandle>,
}

impl<H> HttpApi<H>
where
    H: ApiHandler,
{
    /// Creates an HTTP API for one handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self {
            handler,
            metrics: noop_api_metrics(),
            authenticator: None,
            authorizer: None,
        }
    }

    /// Requires a bearer token on every request.
    ///
    /// The expected header is `Authorization: Bearer <token>`.
    /// Missing or invalid credentials return `401 Unauthorized`.
    /// Rejected requests do not reach the handler.
    /// Authentication is disabled when this method is not called.
    pub fn with_auth(mut self, token: Token) -> Self {
        self.authenticator = Some(Arc::new(StaticBearerAuthenticator::new(token)));
        self
    }

    /// Installs an application-owned bearer authenticator.
    ///
    /// It receives every request before body extraction and returns the [`ApiIdentity`] used by the optional authorizer.
    pub fn with_authenticator(mut self, authenticator: ApiAuthenticatorHandle) -> Self {
        self.authenticator = Some(authenticator);
        self
    }

    /// Installs an application-owned Task API authorization policy.
    ///
    /// The policy runs after wire validation and before the handler operation.
    pub fn with_authorizer(mut self, authorizer: ApiAuthorizerHandle) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Attaches a metrics backend.
    ///
    /// The default backend ignores every update.
    pub fn with_metrics(mut self, metrics: ApiMetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }

    /// Builds a standalone router and its OpenAPI document.
    ///
    /// Every request body is limited to [`MAX_REQUEST_BYTES`].
    /// Optional authentication runs before body extraction.
    /// Optional authorization runs after validation and before the handler.
    /// Metrics include unmatched task API routes and authentication failures.
    pub fn build(self) -> HttpApiParts {
        configure_standalone_openapi_generation();

        let mut openapi = standalone_openapi_document();
        let router = self
            .mount(ApiRouter::new(), &mut openapi)
            .fallback(route_not_found)
            .finish_api(&mut openapi);

        HttpApiParts { router, openapi }
    }

    /// Mounts the documented task API into an application router.
    ///
    /// The task routes are mounted at [`HTTP_API_ROOT`].
    /// Their handler state, access control, limits, metrics, and fallbacks stay inside that route subtree.
    ///
    /// The caller owns the final [`OpenApi`] document.
    /// Call [`ApiRouter::finish_api`] once after every documented service has been mounted.
    pub fn mount<S>(self, app: ApiRouter<S>, openapi: &mut OpenApi) -> ApiRouter<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        let auth_enabled = self.authenticator.is_some();
        let authorization_enabled = self.authorizer.is_some();
        document_task_api(openapi, auth_enabled);

        let mut router = documented_router::<H>(auth_enabled, authorization_enabled)
            .fallback(route_not_found)
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
            .layer(middleware::from_fn(map_413_envelope))
            .layer(Extension(HttpAccessControl {
                authorizer: self.authorizer,
            }));

        if let Some(authenticator) = self.authenticator {
            router = router.layer(middleware::from_fn_with_state(
                authenticator,
                authenticate_bearer,
            ));
        }

        router = router.layer(middleware::from_fn_with_state(
            self.metrics,
            http_metrics_middleware,
        ));

        app.nest_api_service(HTTP_API_ROOT, router.with_state(self.handler))
    }

    /// Builds only the configured axum router.
    ///
    /// Use [`build`](Self::build) when the OpenAPI document is needed.
    pub fn router(self) -> Router {
        self.build().router
    }
}

fn configure_standalone_openapi_generation() {
    aide::generate::reset_context();
    aide::generate::in_context(|context| {
        let settings = SchemaSettings::draft2020_12().with(|settings| {
            settings.inline_subschemas = false;
            settings.definitions_path = "#/components/schemas/".into();
            settings.meta_schema = None;
        });
        context.schema = settings.into_generator();
    });
}

fn documented_router<H>(auth_enabled: bool, authorization_enabled: bool) -> ApiRouter<Arc<H>>
where
    H: ApiHandler,
{
    let tasks = post_with(create_task_route::<H>, move |operation| {
        let operation = operation
            .id("createTask")
            .tag("tasks")
            .summary("Create a task")
            .description(
                "Commits new desired state. See CONTRACT.md for reconciliation semantics.",
            )
            .response::<201, Json<HttpTaskSchema>>()
            .response::<400, ApiError>()
            .response::<409, ApiError>()
            .response::<413, ApiError>()
            .response::<415, ApiError>()
            .response::<429, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .get_with(list_tasks_route::<H>, move |operation| {
        let operation = operation
            .id("listOrWatchTasks")
            .tag("tasks")
            .summary("List or watch tasks")
            .description(
                "Returns one TaskList within the count and 4 MiB response limits unless watch is true. Watch mode emits newline-delimited TaskWatchDocument values. See CONTRACT.md for pagination and resume semantics.",
            )
            .input::<Query<ListTasksParams>>()
            .response::<200, ListOrWatchResponse>()
            .response::<400, ApiError>()
            .response::<410, ApiError>()
            .response::<429, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .fallback_service(method_not_allowed.into_service());

    let task = get_with(get_task_route::<H>, move |operation| {
        let operation = operation
            .id("getTask")
            .tag("tasks")
            .summary("Get a task")
            .response::<200, Json<HttpTaskSchema>>()
            .response::<400, ApiError>()
            .response::<404, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .put_with(apply_task_route::<H>, move |operation| {
        let operation = operation
            .id("applyTask")
            .tag("tasks")
            .summary("Apply desired task state")
            .description(
                "Creates or updates one task. See CONTRACT.md for preconditions and commit semantics.",
            )
            .response::<200, Json<HttpTaskSchema>>()
            .response::<400, ApiError>()
            .response::<404, ApiError>()
            .response::<409, ApiError>()
            .response::<413, ApiError>()
            .response::<415, ApiError>()
            .response::<429, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .delete_with(delete_task_route::<H>, move |operation| {
        let operation = operation
            .id("deleteTask")
            .tag("tasks")
            .summary("Delete a task")
            .description(
                "Requests a terminal logical outcome and removes retained state. \
                 Force-aborted task code can remain physically active.",
            )
            .response::<204, NoContent>()
            .response::<400, ApiError>()
            .response::<404, ApiError>()
            .response::<409, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .fallback_service(method_not_allowed.into_service());

    let runs = get_with(list_task_runs_route::<H>, move |operation| {
        let operation = operation
            .id("listTaskRuns")
            .tag("tasks")
            .summary("List retained task runs")
            .description(
                "Returns one snapshot-consistent page within the count and 4 MiB response limits.",
            )
            .input::<Query<ListTaskRunsParams>>()
            .response::<200, Json<TaskRunList>>()
            .response::<400, ApiError>()
            .response::<404, ApiError>()
            .response::<410, ApiError>()
            .response::<429, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .fallback_service(method_not_allowed.into_service());

    let logs = get_with(stream_task_logs_route::<H>, move |operation| {
        let operation = operation
            .id("streamTaskLogs")
            .tag("tasks")
            .summary("Stream live task output")
            .description(
                "Returns Server-Sent Events. See CONTRACT.md for event names, payloads, and delivery semantics.",
            )
            .response::<200, TaskLogStreamResponse>()
            .response::<400, ApiError>()
            .response::<404, ApiError>();
        document_common_errors(document_access(
            operation,
            auth_enabled,
            authorization_enabled,
        ))
    })
    .fallback_service(method_not_allowed.into_service());

    ApiRouter::new()
        .api_route("/tasks", tasks)
        .api_route("/tasks/{name}", task)
        .api_route("/tasks/{name}/runs", runs)
        .api_route("/tasks/{name}/logs", logs)
}

fn document_auth<'a>(
    operation: TransformOperation<'a>,
    auth_enabled: bool,
) -> TransformOperation<'a> {
    if auth_enabled {
        operation
            .security_requirement(HTTP_BEARER_SCHEME)
            .response::<401, ApiError>()
    } else {
        operation.security_requirement_multi(std::iter::empty::<&str>())
    }
}

fn document_access<'a>(
    operation: TransformOperation<'a>,
    auth_enabled: bool,
    authorization_enabled: bool,
) -> TransformOperation<'a> {
    let operation = document_auth(operation, auth_enabled);
    if authorization_enabled {
        operation.response::<403, ApiError>()
    } else {
        operation
    }
}

fn document_common_errors(operation: TransformOperation<'_>) -> TransformOperation<'_> {
    operation
        .response::<500, ApiError>()
        .response::<503, ApiError>()
}

fn standalone_openapi_document() -> OpenApi {
    OpenApi {
        info: Info {
            title: "Solti Task API".into(),
            summary: Some("Public task transport exposed by one Solti agent.".into()),
            description: Some(
                "OpenAPI describes HTTP shapes and operations. CONTRACT.md defines behavioral semantics."
                    .into(),
            ),
            version: API_VERSION_NAME.into(),
            ..Info::default()
        },
        json_schema_dialect: Some("https://json-schema.org/draft/2020-12/schema".into()),
        ..OpenApi::default()
    }
}

fn document_task_api(openapi: &mut OpenApi, auth_enabled: bool) {
    openapi
        .extensions
        .insert("x-solti-task-api-version".into(), API_VERSION.into());
    openapi.extensions.insert(
        "x-solti-resource-api-version".into(),
        TASK_API_VERSION.into(),
    );
    openapi
        .extensions
        .insert("x-solti-http-api-root".into(), HTTP_API_ROOT.into());
    openapi
        .extensions
        .insert("x-solti-grpc-package".into(), GRPC_API_PACKAGE.into());

    if !openapi.tags.iter().any(|tag| tag.name == "tasks") {
        openapi.tags.push(Tag {
            name: "tasks".into(),
            description: Some("Desired task state, observations, history, and live output.".into()),
            ..Tag::default()
        });
    }

    if auth_enabled {
        let components = openapi.components.get_or_insert_default();
        components.security_schemes.insert(
            HTTP_BEARER_SCHEME.into(),
            ReferenceOr::Item(SecurityScheme::Http {
                scheme: "bearer".into(),
                bearer_format: None,
                description: Some(
                    "Bearer credential configured by the Task API authenticator.".into(),
                ),
                extensions: Default::default(),
            }),
        );
    }
}

#[derive(Clone)]
struct HttpAccessControl {
    authorizer: Option<ApiAuthorizerHandle>,
}

impl HttpAccessControl {
    async fn authorize(
        &self,
        identity: Option<&ApiIdentity>,
        operation: TaskOperation,
        target: TaskTarget<'_>,
    ) -> Result<(), ApiError> {
        let Some(authorizer) = &self.authorizer else {
            return Ok(());
        };
        authorizer
            .authorize(AuthorizationRequest::new(identity, operation, target))
            .await
    }
}

/// Runs the configured bearer authenticator and installs its identity.
async fn authenticate_bearer(
    State(authenticator): State<ApiAuthenticatorHandle>,
    mut req: Request,
    next: Next,
) -> Response {
    let credential = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_value);

    match authenticator
        .authenticate(AuthenticationRequest::new(Transport::Http, credential))
        .await
    {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Debug, Default, JsonSchema)]
#[schemars(rename = "ListTasksQuery", deny_unknown_fields)]
struct ListTasksParams {
    /// Match one slot.
    #[schemars(with = "Option<Slot>")]
    slot: Option<String>,

    /// Match any supplied phase.
    #[schemars(rename = "phase", with = "Option<Vec<TaskPhase>>")]
    phases: Vec<String>,

    /// Match a Kubernetes-style label selector.
    #[schemars(rename = "labelSelector")]
    label_selector: Option<String>,

    /// Page size.
    ///
    /// Omitted or zero means 100.
    /// The maximum is 1000.
    #[schemars(range(max = 1000))]
    limit: Option<u32>,

    /// Opaque continuation token returned by a previous page.
    #[schemars(rename = "continue", with = "Option<NonEmptyStringSchema>")]
    continuation: Option<String>,

    /// Opaque watch start position.
    #[schemars(rename = "resourceVersion", with = "Option<NonEmptyStringSchema>")]
    resource_version: Option<String>,

    /// Select watch mode.
    #[schemars(with = "Option<WatchQuerySchema>")]
    watch: Option<bool>,
}

#[derive(Debug, Default, JsonSchema)]
#[schemars(rename = "ListTaskRunsQuery", deny_unknown_fields)]
struct ListTaskRunsParams {
    /// Page size.
    ///
    /// Omitted or zero means 100.
    /// The maximum is 1000.
    #[schemars(range(max = 1000))]
    limit: Option<u32>,

    /// Opaque continuation token returned by a previous page.
    #[schemars(rename = "continue", with = "Option<NonEmptyStringSchema>")]
    continuation: Option<String>,
}

fn parse_list_tasks_params(raw_query: Option<&str>) -> Result<ListTasksParams, ApiError> {
    let mut params = ListTasksParams::default();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "slot" => set_query_param(&mut params.slot, "slot", value.into_owned())?,
            "phase" => params.phases.push(value.into_owned()),
            "labelSelector" => set_query_param(
                &mut params.label_selector,
                "labelSelector",
                value.into_owned(),
            )?,
            "limit" => {
                let value = parse_u32_query_param("limit", &value)?;
                set_query_param(&mut params.limit, "limit", value)?;
            }
            "continue" => {
                set_query_param(&mut params.continuation, "continue", value.into_owned())?
            }
            "resourceVersion" => set_query_param(
                &mut params.resource_version,
                "resourceVersion",
                value.into_owned(),
            )?,
            "watch" => {
                let value = parse_watch_query_param(&value)?;
                set_query_param(&mut params.watch, "watch", value)?;
            }
            other => {
                return Err(ApiError::InvalidRequest(format!(
                    "unknown query parameter `{other}`"
                )));
            }
        }
    }
    Ok(params)
}

/// Parses strict TaskRun list query parameters.
fn parse_list_task_runs_params(raw_query: Option<&str>) -> Result<ListTaskRunsParams, ApiError> {
    let mut params = ListTaskRunsParams::default();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "limit" => {
                let value = parse_u32_query_param("limit", &value)?;
                set_query_param(&mut params.limit, "limit", value)?;
            }
            "continue" => {
                set_query_param(&mut params.continuation, "continue", value.into_owned())?
            }
            other => {
                return Err(ApiError::InvalidRequest(format!(
                    "unknown query parameter `{other}`"
                )));
            }
        }
    }
    Ok(params)
}

fn parse_watch_query_param(value: &str) -> Result<bool, ApiError> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ApiError::InvalidRequest(
            "query parameter `watch` must be one of: true, false, 1, 0".into(),
        )),
    }
}

fn set_query_param<T>(target: &mut Option<T>, name: &str, value: T) -> Result<(), ApiError> {
    if target.replace(value).is_some() {
        return Err(ApiError::InvalidRequest(format!(
            "query parameter `{name}` must not be repeated"
        )));
    }
    Ok(())
}

fn parse_u32_query_param(name: &str, value: &str) -> Result<u32, ApiError> {
    value.parse().map_err(|_| {
        ApiError::InvalidRequest(format!(
            "query parameter `{name}` must be an unsigned 32-bit integer"
        ))
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteParams {
    /// Require the current resource UID.
    #[schemars(with = "Option<Uid>")]
    uid: Option<String>,

    /// Require the current opaque resource version.
    #[schemars(with = "Option<NonEmptyStringSchema>")]
    resource_version: Option<String>,
}

fn parse_write_preconditions(params: WriteParams) -> Result<WritePreconditions, ApiError> {
    let mut preconditions = WritePreconditions::new();
    if let Some(uid) = params.uid {
        preconditions = preconditions.with_uid(
            Uid::new(uid)
                .map_err(|error| ApiError::InvalidRequest(format!("invalid uid: {error}")))?,
        );
    }
    if let Some(resource_version) = params.resource_version {
        preconditions = preconditions
            .with_resource_version(resource_version)
            .map_err(|error| {
                ApiError::InvalidRequest(format!("invalid resourceVersion: {error}"))
            })?;
    }
    Ok(preconditions)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(deny_unknown_fields)]
struct TaskPath {
    /// Stable task name.
    #[schemars(with = "TaskId")]
    name: String,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(
    rename = "SoltiTaskSpec",
    rename_all = "camelCase",
    deny_unknown_fields
)]
struct HttpTaskSpecSchema {
    slot: Slot,
    workload: HttpTaskWorkloadSchema,
    timeout: Timeout,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    admission: AdmissionPolicy,
    max_retries: Option<NonZeroU32>,
    runner_selector: Option<LabelSelector>,
}

struct HttpTaskWorkloadSchema;

impl JsonSchema for HttpTaskWorkloadSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SoltiTaskWorkload".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let subprocess = http_workload_envelope_schema(
            "Subprocess",
            generator.subschema_for::<SubprocessSpec>(),
        );
        let wasm = http_workload_envelope_schema("Wasm", generator.subschema_for::<WasmSpec>());
        let container =
            http_workload_envelope_schema("Container", generator.subschema_for::<ContainerSpec>());
        let extension = generator.subschema_for::<ExtensionWorkload>();

        schemars::json_schema!({
            "description": "Public workload GVK and desired state. Embedded is in-process only.",
            "oneOf": [subprocess, wasm, container, extension]
        })
    }
}

fn http_workload_envelope_schema(kind: &'static str, spec: Schema) -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["apiVersion", "kind", "spec"],
        "properties": {
            "apiVersion": {
                "type": "string",
                "const": WORKLOAD_API_VERSION
            },
            "kind": {
                "type": "string",
                "const": kind
            },
            "spec": spec
        }
    })
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(
    rename = "SoltiTaskManifest",
    rename_all = "camelCase",
    deny_unknown_fields
)]
struct HttpTaskManifestSchema {
    #[schemars(flatten)]
    type_meta: TypeMeta,
    metadata: TaskManifestMeta,
    spec: HttpTaskSpecSchema,
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(rename = "SoltiTask", rename_all = "camelCase", deny_unknown_fields)]
struct HttpTaskSchema {
    #[schemars(flatten)]
    type_meta: TypeMeta,
    metadata: ObjectMeta,
    spec: HttpTaskSpecSchema,
    status: TaskStatus,
}

#[derive(Debug, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct ListMeta {
    /// Collection version shared by every page in one snapshot.
    resource_version: String,

    /// Opaque continuation token.
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    continuation: Option<String>,

    /// Remaining items after this page.
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_item_count: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskList {
    api_version: &'static str,

    kind: &'static str,

    metadata: ListMeta,
    items: Vec<Task>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
/// Task list envelope with an empty item array for exact size accounting.
struct EmptyTaskList<'a> {
    /// Task API version.
    api_version: &'static str,
    /// Task list kind.
    kind: &'static str,
    /// Candidate page metadata.
    metadata: &'a ListMeta,
    /// Empty item array.
    items: &'a [()],
}

#[derive(Default)]
/// Allocation-free serialized byte counter.
struct SerializedSize(
    /// Counted bytes.
    usize,
);

impl Write for SerializedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(
    rename = "SoltiTaskList",
    rename_all = "camelCase",
    deny_unknown_fields
)]
struct HttpTaskListSchema {
    #[schemars(schema_with = "task_api_version")]
    api_version: &'static str,

    #[schemars(schema_with = "task_list_kind")]
    kind: &'static str,

    metadata: ListMeta,
    items: Vec<HttpTaskSchema>,
}

/// Native HTTP representation of one TaskRun collection page.
#[derive(Debug, JsonSchema, Serialize)]
#[schemars(deny_unknown_fields)]
struct TaskRunList {
    /// Snapshot and continuation metadata.
    metadata: ListMeta,
    /// Complete run items in this prefix.
    runs: Vec<TaskRun>,
}

/// Empty TaskRun list used for exact envelope-size measurement.
#[derive(Serialize)]
struct EmptyTaskRunList<'a> {
    /// Candidate metadata for the measured prefix.
    metadata: &'a ListMeta,
    /// Empty item slice with the final field shape.
    runs: &'a [()],
}

#[derive(Serialize)]
#[serde(tag = "type", content = "object")]
enum TaskWatchDocument {
    #[serde(rename = "ADDED")]
    Added(Task),
    #[serde(rename = "MODIFIED")]
    Modified(Task),
    #[serde(rename = "DELETED")]
    Deleted(Task),
    #[serde(rename = "ERROR")]
    Error(HttpStatusResource),
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[schemars(rename = "SoltiTaskWatchDocument", tag = "type", content = "object")]
enum HttpTaskWatchDocumentSchema {
    #[schemars(rename = "ADDED")]
    Added(HttpTaskSchema),
    #[schemars(rename = "MODIFIED")]
    Modified(HttpTaskSchema),
    #[schemars(rename = "DELETED")]
    Deleted(HttpTaskSchema),
    #[schemars(rename = "ERROR")]
    Error(HttpStatusResource),
}

struct ListOrWatchResponse;

impl OperationOutput for ListOrWatchResponse {
    type Inner = serde_json::Value;

    fn operation_response(
        context: &mut GenContext,
        _operation: &mut aide::openapi::Operation,
    ) -> Option<ApiResponse> {
        let list = context.schema.subschema_for::<HttpTaskListSchema>();
        let watch_document = context
            .schema
            .subschema_for::<HttpTaskWatchDocumentSchema>();
        Some(ApiResponse {
            description: "A TaskList, or a newline-delimited sequence of TaskWatchDocument values when watch is true.".into(),
            content: [(
                "application/json".into(),
                media_type(schemars::json_schema!({
                    "description": "List and watch share one HTTP media type. See CONTRACT.md for stream framing.",
                    "oneOf": [list, watch_document]
                })),
            )]
            .into_iter()
            .collect(),
            ..ApiResponse::default()
        })
    }
}

struct TaskLogStreamResponse;

impl OperationOutput for TaskLogStreamResponse {
    type Inner = serde_json::Value;

    fn operation_response(
        context: &mut GenContext,
        _operation: &mut aide::openapi::Operation,
    ) -> Option<ApiResponse> {
        Some(ApiResponse {
            description:
                "Server-Sent Events. Each data field is JSON matching OutputEvent. See CONTRACT.md for framing and delivery semantics."
                    .into(),
            content: [(
                "text/event-stream".into(),
                media_type(context.schema.subschema_for::<OutputEvent>()),
            )]
            .into_iter()
            .collect(),
            ..ApiResponse::default()
        })
    }
}

impl OperationOutput for ApiError {
    type Inner = serde_json::Value;

    fn operation_response(
        context: &mut GenContext,
        _operation: &mut aide::openapi::Operation,
    ) -> Option<ApiResponse> {
        Some(json_response::<HttpStatusResource>(
            context,
            "Kubernetes-style Status failure.",
        ))
    }
}

fn json_response<T>(context: &mut GenContext, description: &str) -> ApiResponse
where
    T: JsonSchema,
{
    ApiResponse {
        description: description.into(),
        content: [(
            "application/json".into(),
            media_type(context.schema.subschema_for::<T>()),
        )]
        .into_iter()
        .collect(),
        ..ApiResponse::default()
    }
}

fn media_type(schema: Schema) -> MediaType {
    MediaType {
        schema: Some(SchemaObject {
            json_schema: schema,
            example: None,
            external_docs: None,
        }),
        ..MediaType::default()
    }
}

struct WatchQuerySchema;

impl JsonSchema for WatchQuerySchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WatchQuery".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "boolean"
                },
                {
                    "type": "string",
                    "enum": ["0", "1"]
                }
            ]
        })
    }
}

struct NonEmptyStringSchema;

impl JsonSchema for NonEmptyStringSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NonEmptyString".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "pattern": "\\S"
        })
    }
}

fn task_api_version(_generator: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "const": TASK_API_VERSION
    })
}

fn task_list_kind(_generator: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "const": "TaskList"
    })
}

struct TaskWatchBodyStream {
    source: TaskWatchEventStream,
    terminated: bool,
}

impl TaskWatchBodyStream {
    fn new(source: TaskWatchEventStream) -> Self {
        Self {
            source,
            terminated: false,
        }
    }
}

impl Stream for TaskWatchBodyStream {
    type Item = Result<Vec<u8>, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        match self.source.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                let event = event.and_then(public_watch_event);
                self.terminated = event.is_err();
                Poll::Ready(Some(Ok(encode_watch_document(event))))
            }
            Poll::Ready(None) => {
                self.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn reject_embedded_manifest(manifest: &TaskManifest) -> Result<(), ApiError> {
    if !manifest_is_visible(manifest) {
        return Err(ApiError::InvalidRequest(
            "Embedded workloads are available only through the in-process SDK".into(),
        ));
    }
    Ok(())
}

fn public_task(task: Task) -> Result<Task, ApiError> {
    if !task_is_visible(&task) {
        return Err(ApiError::Internal(
            "handler returned an Embedded workload through the public HTTP API".into(),
        ));
    }
    Ok(task)
}

fn build_task_filter(
    slot: Option<String>,
    phases: Vec<String>,
    label_selector: Option<String>,
) -> Result<TaskFilter, ApiError> {
    let mut filter = TaskFilter::new();

    if let Some(slot) = slot {
        filter = filter.with_slot(validate_slot(slot)?);
    }

    for phase_str in phases {
        let phase = phase_str.parse::<TaskPhase>().map_err(|_| {
            ApiError::InvalidRequest(format!(
                "invalid phase: '{phase_str}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)"
            ))
        })?;
        filter = filter.with_phase(phase);
    }

    if let Some(label_selector) = label_selector {
        let selector = label_selector
            .parse::<LabelSelector>()
            .map_err(|error| ApiError::InvalidRequest(format!("invalid labelSelector: {error}")))?;
        filter = filter
            .with_label_selector(selector)
            .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    }

    Ok(filter)
}

fn public_watch_event(event: TaskWatchEvent) -> Result<TaskWatchEvent, ApiError> {
    if !task_is_visible(event.object()) {
        return Err(ApiError::Internal(
            "handler returned an Embedded workload through the public HTTP watch".into(),
        ));
    }
    Ok(event)
}

fn encode_watch_document(event: Result<TaskWatchEvent, ApiError>) -> Vec<u8> {
    let document = match event {
        Ok(TaskWatchEvent::Added(task)) => TaskWatchDocument::Added(task),
        Ok(TaskWatchEvent::Modified(task)) => TaskWatchDocument::Modified(task),
        Ok(TaskWatchEvent::Deleted(task)) => TaskWatchDocument::Deleted(task),
        Err(error) => {
            let (_, status) = error.into_http_status();
            TaskWatchDocument::Error(status)
        }
    };

    match serde_json::to_vec(&document) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            tracing::error!(
                event = "api.internal_error",
                transport = "http",
                operation = "watch_tasks",
                stage = "serialize_event",
                %error,
                "failed to serialize task watch event"
            );
            br#"{"type":"ERROR","object":{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Failure","message":"internal server error","reason":"InternalError","code":500}}
"#
            .to_vec()
        }
    }
}

async fn create_task_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    manifest: TaskManifestJson,
) -> NoApi<Result<(StatusCode, Json<Task>), ApiError>>
where
    H: ApiHandler,
{
    NoApi(create_task(state, &access, identity.as_deref(), manifest).await)
}

async fn apply_task_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    path: ApiPath<TaskPath>,
    query: ApiQuery<WriteParams>,
    manifest: TaskManifestJson,
) -> NoApi<Result<Json<Task>, ApiError>>
where
    H: ApiHandler,
{
    NoApi(apply_task(state, &access, identity.as_deref(), path, query, manifest).await)
}

async fn get_task_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    path: ApiPath<TaskPath>,
) -> NoApi<Result<Json<Task>, ApiError>>
where
    H: ApiHandler,
{
    NoApi(get_task(state, &access, identity.as_deref(), path).await)
}

async fn list_tasks_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    query: RawQuery,
) -> NoApi<Result<Response, ApiError>>
where
    H: ApiHandler,
{
    NoApi(list_tasks(state, &access, identity.as_deref(), query).await)
}

async fn list_task_runs_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    path: ApiPath<TaskPath>,
    query: RawQuery,
) -> NoApi<Result<Response, ApiError>>
where
    H: ApiHandler,
{
    NoApi(list_task_runs(state, &access, identity.as_deref(), path, query).await)
}

async fn delete_task_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    path: ApiPath<TaskPath>,
    query: ApiQuery<WriteParams>,
) -> NoApi<Result<NoContent, ApiError>>
where
    H: ApiHandler,
{
    NoApi(delete_task(state, &access, identity.as_deref(), path, query).await)
}

async fn stream_task_logs_route<H>(
    state: State<Arc<H>>,
    Extension(access): Extension<HttpAccessControl>,
    identity: Option<Extension<ApiIdentity>>,
    path: ApiPath<TaskPath>,
) -> NoApi<Result<Response, ApiError>>
where
    H: ApiHandler,
{
    NoApi(stream_task_logs(state, &access, identity.as_deref(), path).await)
}

async fn create_task<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    TaskManifestJson(manifest): TaskManifestJson,
) -> Result<(StatusCode, Json<Task>), ApiError>
where
    H: ApiHandler,
{
    reject_embedded_manifest(&manifest)?;
    access
        .authorize(
            identity,
            TaskOperation::Create,
            TaskTarget::Manifest(&manifest),
        )
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "create",
        task_name = %manifest.name(),
        "task operation started"
    );
    let task = public_task(handler.create_task(manifest).await?)?;
    Ok((StatusCode::CREATED, Json(task)))
}

async fn apply_task<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    ApiPath(TaskPath { name: path_name }): ApiPath<TaskPath>,
    ApiQuery(params): ApiQuery<WriteParams>,
    TaskManifestJson(manifest): TaskManifestJson,
) -> Result<Json<Task>, ApiError>
where
    H: ApiHandler,
{
    let path_name = parse_task_id("task name", path_name)?;
    reject_embedded_manifest(&manifest)?;
    if manifest.name() != &path_name {
        return Err(ApiError::InvalidRequest(format!(
            "path task name `{path_name}` does not match metadata.name `{}`",
            manifest.name()
        )));
    }
    let preconditions = parse_write_preconditions(params)?;
    access
        .authorize(
            identity,
            TaskOperation::Apply,
            TaskTarget::Manifest(&manifest),
        )
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "apply",
        task_name = %manifest.name(),
        "task operation started"
    );
    let task = public_task(handler.apply_task(manifest, preconditions).await?)?;
    Ok(Json(task))
}

async fn get_task<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    ApiPath(TaskPath { name }): ApiPath<TaskPath>,
) -> Result<Json<Task>, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    access
        .authorize(identity, TaskOperation::Get, TaskTarget::Task(&name))
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "get",
        task_name = %name,
        "task operation started"
    );
    let task = handler
        .get_task(&name)
        .await?
        .ok_or_else(|| ApiError::TaskNotFound(name.to_string()))?;

    Ok(Json(public_task(task)?))
}

/// Builds HTTP collection metadata for one item prefix.
fn list_meta_for_prefix(
    page: &solti_model::TaskPage<Task>,
    filter: &TaskFilter,
    keep: usize,
) -> Result<ListMeta, ApiError> {
    let (continuation, remaining_item_count) =
        crate::continuation::prefix_metadata(page, filter, keep)?;
    Ok(ListMeta {
        resource_version: page.resource_version.clone(),
        continuation: continuation.map(crate::continuation::encode).transpose()?,
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Measures one compact JSON value without retaining encoded bytes.
fn compact_json_size(value: &impl Serialize) -> Result<usize, ApiError> {
    let mut size = SerializedSize::default();
    serde_json::to_writer(&mut size, value)
        .map_err(|error| ApiError::Internal(format!("measure JSON response: {error}")))?;
    Ok(size.0)
}

/// Calculates the complete Task list JSON body size.
fn task_list_size(metadata: &ListMeta, serialized_item_bytes: usize) -> Result<usize, ApiError> {
    let empty = EmptyTaskList {
        api_version: TASK_API_VERSION,
        kind: "TaskList",
        metadata,
        items: &[],
    };
    compact_json_size(&empty).map(|bytes| bytes.saturating_add(serialized_item_bytes))
}

/// Encodes the largest complete prefix from one domain page within the HTTP limit.
fn encode_bounded_task_list(
    page: solti_model::TaskPage<Task>,
    filter: &TaskFilter,
) -> Result<(Response, usize, usize), ApiError> {
    encode_bounded_task_list_with_limit(page, filter, crate::MAX_TASK_LIST_RESPONSE_BYTES)
}

/// Encodes a complete Task prefix within an explicit testable limit.
fn encode_bounded_task_list_with_limit(
    page: solti_model::TaskPage<Task>,
    filter: &TaskFilter,
    limit: usize,
) -> Result<(Response, usize, usize), ApiError> {
    let mut serialized_item_bytes = 0usize;
    let mut keep = None;

    if page.items.is_empty() {
        keep = Some(0);
    } else {
        for (index, task) in page.items.iter().enumerate() {
            let task_bytes = compact_json_size(task)?;
            serialized_item_bytes = serialized_item_bytes
                .saturating_add(usize::from(index > 0))
                .saturating_add(task_bytes);
            let candidate = index + 1;
            let metadata = list_meta_for_prefix(&page, filter, candidate)?;
            if task_list_size(&metadata, serialized_item_bytes)? <= limit {
                keep = Some(candidate);
            }
        }
    }

    let keep = keep.ok_or_else(|| {
        ApiError::ResourceExhausted(format!(
            "the first Task exceeds the {limit}-byte list response limit"
        ))
    })?;
    let page = crate::continuation::retain_prefix(page, filter, keep)?;
    let count = page.items.len();
    let remaining = page.remaining_item_count;
    let metadata = list_meta_for_prefix(&page, filter, count)?;
    let body = serde_json::to_vec(&TaskList {
        api_version: TASK_API_VERSION,
        kind: "TaskList",
        metadata,
        items: page.items,
    })
    .map_err(|error| ApiError::Internal(format!("encode Task list response: {error}")))?;
    if body.len() > limit {
        return Err(ApiError::Internal(
            "bounded Task list exceeded its encoded response limit".into(),
        ));
    }
    let mut response = Body::from(body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok((response, count, remaining))
}

/// Builds HTTP metadata for one retained TaskRun prefix.
fn run_list_meta_for_prefix(page: &TaskRunPage, keep: usize) -> Result<ListMeta, ApiError> {
    let (continuation, remaining_item_count) =
        crate::continuation::run_prefix_metadata(page, keep)?;
    Ok(ListMeta {
        resource_version: page.resource_version.clone(),
        continuation: continuation
            .map(crate::continuation::encode_run)
            .transpose()?,
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Returns the exact compact JSON size for a TaskRun response candidate.
fn task_run_list_size(
    metadata: &ListMeta,
    serialized_item_bytes: usize,
) -> Result<usize, ApiError> {
    let empty = EmptyTaskRunList {
        metadata,
        runs: &[],
    };
    compact_json_size(&empty).map(|bytes| bytes.saturating_add(serialized_item_bytes))
}

/// Encodes the largest complete TaskRun prefix within the public wire limit.
fn encode_bounded_task_run_list(page: TaskRunPage) -> Result<(Response, usize, usize), ApiError> {
    encode_bounded_task_run_list_with_limit(page, crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES)
}

/// Encodes the largest complete TaskRun prefix within `limit` bytes.
fn encode_bounded_task_run_list_with_limit(
    page: TaskRunPage,
    limit: usize,
) -> Result<(Response, usize, usize), ApiError> {
    let mut serialized_item_bytes = 0usize;
    let mut keep = page.items.is_empty().then_some(0);

    for (index, run) in page.items.iter().enumerate() {
        let run_bytes = compact_json_size(run)?;
        serialized_item_bytes = serialized_item_bytes
            .saturating_add(usize::from(index > 0))
            .saturating_add(run_bytes);
        let candidate = index + 1;
        let metadata = run_list_meta_for_prefix(&page, candidate)?;
        if task_run_list_size(&metadata, serialized_item_bytes)? <= limit {
            keep = Some(candidate);
        }
    }

    let keep = keep.ok_or_else(|| {
        ApiError::ResourceExhausted(format!(
            "the first TaskRun exceeds the {limit}-byte list response limit"
        ))
    })?;
    let page = crate::continuation::retain_run_prefix(page, keep)?;
    let count = page.items.len();
    let remaining = page.remaining_item_count;
    let metadata = run_list_meta_for_prefix(&page, count)?;
    let body = serde_json::to_vec(&TaskRunList {
        metadata,
        runs: page.items,
    })
    .map_err(|error| ApiError::Internal(format!("encode TaskRun list response: {error}")))?;
    if body.len() > limit {
        return Err(ApiError::Internal(
            "bounded TaskRun list exceeded its encoded response limit".into(),
        ));
    }
    let mut response = Body::from(body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok((response, count, remaining))
}

async fn list_tasks<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError>
where
    H: ApiHandler,
{
    let ListTasksParams {
        slot,
        phases,
        label_selector,
        limit,
        continuation,
        resource_version,
        watch,
    } = parse_list_tasks_params(raw_query.as_deref())?;
    let filter = build_task_filter(slot, phases, label_selector)?;

    if watch.unwrap_or(false) {
        if limit.is_some() || continuation.is_some() {
            return Err(ApiError::InvalidRequest(
                "query parameters `limit` and `continue` are not supported for watch".into(),
            ));
        }
        if resource_version
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ApiError::InvalidRequest(
                "query parameter `resourceVersion` must not be empty".into(),
            ));
        }

        access
            .authorize(identity, TaskOperation::Watch, TaskTarget::Collection)
            .await?;
        debug!(
            event = "api.operation",
            transport = "http",
            operation = "watch",
            "task operation started"
        );
        let stream = handler.watch_tasks(filter, resource_version).await?;
        let body_stream = TaskWatchBodyStream::new(stream);
        let mut response = Body::from_stream(body_stream).into_response();
        response.extensions_mut().insert(StreamingResponse);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return Ok(response);
    }

    if resource_version.is_some() {
        return Err(ApiError::InvalidRequest(
            "query parameter `resourceVersion` requires `watch=true`".into(),
        ));
    }

    let mut query = TaskQuery::from_filter(filter);
    query = query
        .with_limit(parse_list_limit(limit.unwrap_or(0))?)
        .with_item_byte_limit(
            NonZeroUsize::new(crate::MAX_TASK_LIST_RESPONSE_BYTES)
                .expect("the Task list response limit is positive"),
        );
    if let Some(token) = continuation {
        if token.is_empty() {
            return Err(ApiError::InvalidRequest(
                "query parameter `continue` must not be empty".into(),
            ));
        }
        let continuation = crate::continuation::decode(&token)?;
        crate::continuation::validate_continuation_filter(&continuation, query.filter())?;
        query = query.with_continuation(continuation);
    }

    let validation_query = query.clone();
    access
        .authorize(identity, TaskOperation::List, TaskTarget::Collection)
        .await?;
    let page = handler.query_tasks(query).await?;
    crate::continuation::validate_page(&page, &validation_query)?;
    for task in &page.items {
        if !task_is_visible(task) {
            return Err(ApiError::Internal(
                "handler returned an Embedded workload through the public HTTP API".into(),
            ));
        }
    }

    let (response, count, remaining) = encode_bounded_task_list(page, validation_query.filter())?;
    debug!(
        event = "api.operation_completed",
        transport = "http",
        operation = "list",
        count,
        remaining,
        "task operation completed"
    );
    Ok(response)
}

async fn list_task_runs<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    ApiPath(TaskPath { name }): ApiPath<TaskPath>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError>
where
    H: ApiHandler,
{
    let ListTaskRunsParams {
        limit,
        continuation,
    } = parse_list_task_runs_params(raw_query.as_deref())?;
    let name = parse_task_id("task name", name)?;
    let mut query = TaskRunQuery::new()
        .with_limit(parse_task_run_limit(limit.unwrap_or(0))?)
        .with_item_byte_limit(
            NonZeroUsize::new(crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES)
                .expect("the TaskRun list response limit is positive"),
        );
    if let Some(token) = continuation {
        if token.is_empty() {
            return Err(ApiError::InvalidRequest(
                "query parameter `continue` must not be empty".into(),
            ));
        }
        let continuation = crate::continuation::decode_run(&token)?;
        crate::continuation::validate_run_continuation_task(&continuation, &name)?;
        query = query.with_continuation(continuation);
    }
    let validation_query = query.clone();
    access
        .authorize(identity, TaskOperation::ListRuns, TaskTarget::Task(&name))
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "list_runs",
        task_name = %name,
        "task operation started"
    );
    let page = handler.query_task_runs(&name, query).await?;
    crate::continuation::validate_run_page(&page, &name, &validation_query)?;
    let (response, count, remaining) = encode_bounded_task_run_list(page)?;
    debug!(
        event = "api.operation_completed",
        transport = "http",
        operation = "list_runs",
        task_name = %name,
        count,
        remaining,
        "task operation completed"
    );
    Ok(response)
}

async fn delete_task<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    ApiPath(TaskPath { name }): ApiPath<TaskPath>,
    ApiQuery(params): ApiQuery<WriteParams>,
) -> Result<NoContent, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    let preconditions = parse_write_preconditions(params)?;
    access
        .authorize(identity, TaskOperation::Delete, TaskTarget::Task(&name))
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "delete",
        task_name = %name,
        "task operation started"
    );
    handler.delete_task(&name, preconditions).await?;

    Ok(NoContent)
}

/// Streams task output as Server-Sent Events.
async fn stream_task_logs<H>(
    State(handler): State<Arc<H>>,
    access: &HttpAccessControl,
    identity: Option<&ApiIdentity>,
    ApiPath(TaskPath { name }): ApiPath<TaskPath>,
) -> Result<Response, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    access
        .authorize(identity, TaskOperation::StreamLogs, TaskTarget::Task(&name))
        .await?;
    debug!(
        event = "api.operation",
        transport = "http",
        operation = "stream_logs",
        task_name = %name,
        "task operation started"
    );
    let stream = handler.stream_task_logs(&name).await?;

    let sse_stream = stream.map(|ev| {
        let name = match &ev {
            OutputEvent::Chunk(_) => "chunk",
            OutputEvent::RunStarted { .. } => "run-started",
            OutputEvent::RunFinished { .. } => "run-finished",
            OutputEvent::Lagged { .. } => "lagged",
            _ => "unknown",
        };
        let data = serde_json::to_string(&ev).map_err(|error| {
            ApiError::Internal(format!("failed to serialize output event: {error}"))
        })?;
        Ok::<Event, ApiError>(Event::default().event(name).data(data))
    });
    let mut response = Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.extensions_mut().insert(StreamingResponse);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    use http_body_util::BodyExt as _;
    use solti_model::{
        Flag, SubprocessMode, TaskEnv, TaskPage, TaskSpec, TaskWorkload, WorkloadTypeMeta,
    };

    fn task(name: &str) -> Task {
        let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: Vec::new(),
            },
            TaskEnv::new(),
            None,
            Flag::enabled(),
        ));
        let spec = TaskSpec::builder("slot", workload, 1_000_u64)
            .build()
            .unwrap();
        Task::new(name, spec).unwrap()
    }

    fn page() -> solti_model::TaskPage<Task> {
        TaskPage {
            items: vec![task("a-first"), task("b-second")],
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 0,
        }
    }

    fn run_page() -> TaskRunPage {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        TaskRunPage {
            items: vec![
                TaskRun::starting(1, 1, workload.clone()).unwrap(),
                TaskRun::from_parts(
                    workload,
                    1,
                    2,
                    TaskPhase::Failed,
                    std::time::UNIX_EPOCH + std::time::Duration::from_millis(1),
                    Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(2)),
                    Some("x".repeat(1_024)),
                    Some(1),
                )
                .unwrap(),
            ],
            task: TaskId::new("task-1").unwrap(),
            task_uid: Uid::new("task-1-uid").unwrap(),
            resource_version: "runs-test:2".into(),
            continuation: None,
            remaining_item_count: 0,
        }
    }

    #[tokio::test]
    async fn task_list_json_stops_at_the_exact_encoded_boundary() {
        let page = page();
        let filter = TaskFilter::new();
        let first_items = compact_json_size(&page.items[0]).unwrap();
        let first_meta = list_meta_for_prefix(&page, &filter, 1).unwrap();
        let first_limit = task_list_size(&first_meta, first_items).unwrap();

        let (response, count, remaining) =
            encode_bounded_task_list_with_limit(page.clone(), &filter, first_limit).unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(bytes.len(), first_limit);
        assert_eq!(count, 1);
        assert_eq!(remaining, 1);
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert!(json["metadata"]["continue"].is_string());
        assert_eq!(json["metadata"]["remainingItemCount"], 1);

        let all_items = first_items + 1 + compact_json_size(&page.items[1]).unwrap();
        let all_meta = list_meta_for_prefix(&page, &filter, 2).unwrap();
        let all_limit = task_list_size(&all_meta, all_items).unwrap();
        let (response, count, remaining) =
            encode_bounded_task_list_with_limit(page, &filter, all_limit).unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.len(), all_limit);
        assert_eq!(count, 2);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn first_task_larger_than_the_json_response_limit_is_typed() {
        let page = page();
        let filter = TaskFilter::new();
        let first_items = compact_json_size(&page.items[0]).unwrap();
        let first_meta = list_meta_for_prefix(&page, &filter, 1).unwrap();
        let first_limit = task_list_size(&first_meta, first_items).unwrap();

        assert!(matches!(
            encode_bounded_task_list_with_limit(page, &filter, first_limit - 1),
            Err(ApiError::ResourceExhausted(_))
        ));
    }

    #[tokio::test]
    async fn task_run_list_json_stops_at_the_exact_encoded_boundary() {
        let page = run_page();
        let first_items = compact_json_size(&page.items[0]).unwrap();
        let first_meta = run_list_meta_for_prefix(&page, 1).unwrap();
        let first_limit = task_run_list_size(&first_meta, first_items).unwrap();

        let (response, count, remaining) =
            encode_bounded_task_run_list_with_limit(page.clone(), first_limit).unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(bytes.len(), first_limit);
        assert_eq!(count, 1);
        assert_eq!(remaining, 1);
        assert_eq!(json["runs"].as_array().unwrap().len(), 1);
        assert_eq!(json["metadata"]["remainingItemCount"], 1);
        let continuation =
            crate::continuation::decode_run(json["metadata"]["continue"].as_str().unwrap())
                .unwrap();
        assert_eq!(continuation.task().as_str(), "task-1");
        assert_eq!(continuation.task_uid().as_str(), "task-1-uid");
        assert_eq!(continuation.after_attempt(), 1);

        assert!(matches!(
            encode_bounded_task_run_list_with_limit(page, first_limit - 1),
            Err(ApiError::ResourceExhausted(_))
        ));
    }
}
