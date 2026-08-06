//! Integration tests for the HTTP transport.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{
    ApiAuthorizer, ApiError, ApiHandler, ApiMetricsBackend, AuthorizationRequest, HttpApi,
    TaskWatchEventStream, Transport,
};
use solti_model::{
    EmbeddedSpec, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRun,
    TaskWorkload, Token, WritePreconditions,
};

#[derive(Default)]
struct MockHandler {
    submit_calls: AtomicUsize,
    delete_calls: AtomicUsize,
    delete_returns_not_found: bool,
}

#[async_trait]
impl ApiHandler for MockHandler {
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
    }

    async fn apply_task(
        &self,
        manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
    }

    async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(None)
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(TaskPage {
            items: Vec::new(),
            resource_version: "test:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn watch_tasks(
        &self,
        _filter: TaskFilter,
        _resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        Ok(Box::pin(tokio_stream::empty()))
    }

    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        if id.as_str() == "runs-missing" {
            Err(ApiError::TaskNotFound(id.to_string()))
        } else {
            Ok(Vec::new())
        }
    }

    async fn delete_task(
        &self,
        id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.delete_returns_not_found {
            Err(ApiError::TaskNotFound(id.to_string()))
        } else {
            Ok(())
        }
    }

    async fn stream_task_logs(
        &self,
        id: &TaskId,
    ) -> Result<solti_api::OutputEventStream, ApiError> {
        // Mock surface: return a fixed two-event stream so the SSE handler
        // has something deterministic to render. Real adapter feeds this
        // from the core output subscription.
        use std::time::{Duration, UNIX_EPOCH};

        use bytes::Bytes;
        use solti_model::{OutputChunk, OutputEvent, StreamKind};

        if id.as_str() == "stream-missing" {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        if id.as_str() == "stream-pending" {
            return Ok(Box::pin(tokio_stream::pending()));
        }

        let events = vec![
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1000),
            },
            OutputEvent::Chunk(OutputChunk {
                generation: 1,
                attempt: 1,
                stream: StreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1100),
                line: Bytes::from_static(b"hello-from-mock"),
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn router_with(handler: Arc<MockHandler>) -> axum::Router {
    HttpApi::new(handler).router()
}

#[derive(Debug, Default)]
struct MetricsProbe {
    in_flight: AtomicI64,
    completed: AtomicUsize,
    last_status: AtomicU16,
}

impl ApiMetricsBackend for MetricsProbe {
    fn record_request(
        &self,
        _transport: Transport,
        _method: &str,
        _path: &str,
        status: u16,
        _duration_ms: u64,
    ) {
        self.last_status.store(status, Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    fn record_in_flight_delta(&self, _transport: Transport, delta: i64) {
        self.in_flight.fetch_add(delta, Ordering::SeqCst);
    }
}

fn router_with_metrics(handler: Arc<MockHandler>, metrics: Arc<MetricsProbe>) -> axum::Router {
    HttpApi::new(handler).with_metrics(metrics).router()
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body must be valid json")
}

fn contains_const(value: &Value, expected: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_const(value, expected)),
        Value::Object(values) => {
            values.get("const").and_then(Value::as_str) == Some(expected)
                || values.values().any(|value| contains_const(value, expected))
        }
        _ => false,
    }
}

fn assert_status(body: &Value, reason: &str, code: u16) {
    assert_eq!(body["apiVersion"], "v1");
    assert_eq!(body["kind"], "Status");
    assert_eq!(body["metadata"], serde_json::json!({}));
    assert_eq!(body["status"], "Failure");
    assert_eq!(body["reason"], reason);
    assert_eq!(body["code"], code);
    assert!(body["message"].is_string());
}

#[test]
fn generated_openapi_describes_the_installed_task_routes() {
    let parts = HttpApi::new(Arc::new(MockHandler::default())).build();
    let document = serde_json::to_value(parts.openapi).unwrap();

    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["jsonSchemaDialect"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(document["info"]["version"], solti_api::API_VERSION_NAME);

    let paths = &document["paths"];
    let tasks = &paths["/apis/solti.io/v1/tasks"];
    let task = &paths["/apis/solti.io/v1/tasks/{name}"];
    let runs = &paths["/apis/solti.io/v1/tasks/{name}/runs"];
    let logs = &paths["/apis/solti.io/v1/tasks/{name}/logs"];

    assert_eq!(tasks["post"]["operationId"], "createTask");
    assert_eq!(tasks["get"]["operationId"], "listOrWatchTasks");
    assert_eq!(task["get"]["operationId"], "getTask");
    assert_eq!(task["put"]["operationId"], "applyTask");
    assert_eq!(task["delete"]["operationId"], "deleteTask");
    assert_eq!(runs["get"]["operationId"], "listTaskRuns");
    assert_eq!(logs["get"]["operationId"], "streamTaskLogs");

    assert_eq!(
        tasks["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/SoltiTaskManifest"
    );
    assert_eq!(
        tasks["post"]["responses"]["400"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/HttpStatusResource"
    );
    assert!(tasks["post"]["responses"].get("200").is_none());
    assert!(tasks["post"]["responses"].get("422").is_none());

    let list_content = tasks["get"]["responses"]["200"]["content"]
        .as_object()
        .unwrap();
    assert_eq!(
        list_content.keys().map(String::as_str).collect::<Vec<_>>(),
        ["application/json"]
    );
    let log_content = logs["get"]["responses"]["200"]["content"]
        .as_object()
        .unwrap();
    assert_eq!(
        log_content.keys().map(String::as_str).collect::<Vec<_>>(),
        ["text/event-stream"]
    );

    let list_parameters = tasks["get"]["parameters"].as_array().unwrap();
    for name in [
        "slot",
        "phase",
        "labelSelector",
        "limit",
        "continue",
        "resourceVersion",
        "watch",
    ] {
        assert!(
            list_parameters
                .iter()
                .any(|parameter| parameter["name"] == name),
            "missing list parameter {name}"
        );
    }
    for name in ["slot", "phase", "continue", "resourceVersion", "watch"] {
        let parameter = list_parameters
            .iter()
            .find(|parameter| parameter["name"] == name)
            .unwrap();
        assert!(
            parameter.get("required").is_none(),
            "optional list parameter {name} is documented as required"
        );
    }
    let slot = list_parameters
        .iter()
        .find(|parameter| parameter["name"] == "slot")
        .unwrap();
    assert_eq!(slot["schema"]["$ref"], "#/components/schemas/Slot");
    for name in ["continue", "resourceVersion"] {
        let parameter = list_parameters
            .iter()
            .find(|parameter| parameter["name"] == name)
            .unwrap();
        assert_eq!(parameter["schema"]["minLength"], 1);
        assert_eq!(parameter["schema"]["pattern"], "\\S");
    }

    let get_parameters = task["get"]["parameters"].as_array().unwrap();
    let name = get_parameters
        .iter()
        .find(|parameter| parameter["name"] == "name")
        .unwrap();
    assert_eq!(name["in"], "path");
    assert_eq!(name["required"], true);
    assert_eq!(name["schema"]["$ref"], "#/components/schemas/TaskId");
    assert_eq!(
        document["components"]["schemas"]["TaskId"]["pattern"],
        "^[a-z0-9](?:[-a-z0-9]*[a-z0-9])?(?:\\.[a-z0-9](?:[-a-z0-9]*[a-z0-9])?)*$"
    );

    let write_parameters = task["put"]["parameters"].as_array().unwrap();
    let uid = write_parameters
        .iter()
        .find(|parameter| parameter["name"] == "uid")
        .unwrap();
    assert!(uid.get("required").is_none());
    assert_eq!(uid["schema"]["$ref"], "#/components/schemas/Uid");
    let resource_version = write_parameters
        .iter()
        .find(|parameter| parameter["name"] == "resourceVersion")
        .unwrap();
    assert!(resource_version.get("required").is_none());
    assert_eq!(resource_version["schema"]["minLength"], 1);
    assert_eq!(resource_version["schema"]["pattern"], "\\S");

    let schemas = &document["components"]["schemas"];
    assert!(schemas.get("SoltiTaskManifest").is_some());
    assert!(schemas.get("SoltiTask").is_some());
    assert!(schemas.get("HttpStatusResource").is_some());
    assert!(schemas.get("EmbeddedSpec").is_none());
    assert!(!contains_const(&schemas["SoltiTaskWorkload"], "Embedded"));
    assert!(document.get("security").is_none());
    assert_eq!(tasks["post"]["security"], serde_json::json!([{}]));
    assert!(tasks["post"]["responses"].get("401").is_none());
}

#[test]
fn generated_openapi_reflects_configured_authentication() {
    let token = Token::new("test-secret").unwrap();
    let document = serde_json::to_value(
        HttpApi::new(Arc::new(MockHandler::default()))
            .with_auth(token)
            .build()
            .openapi,
    )
    .unwrap();

    let bearer = &document["components"]["securitySchemes"]["soltiTaskBearer"];
    assert_eq!(bearer["type"], "http");
    assert_eq!(bearer["scheme"], "bearer");
    assert_eq!(
        document["paths"]["/apis/solti.io/v1/tasks"]["post"]["security"],
        serde_json::json!([{ "soltiTaskBearer": [] }])
    );
    assert!(document.get("security").is_none());
    assert!(
        document["paths"]["/apis/solti.io/v1/tasks"]["post"]["responses"]
            .get("401")
            .is_some()
    );
}

struct AllowAuthorizer;

#[async_trait]
impl ApiAuthorizer for AllowAuthorizer {
    async fn authorize(&self, _request: AuthorizationRequest<'_>) -> Result<(), ApiError> {
        Ok(())
    }
}

#[test]
fn generated_openapi_reflects_configured_authorization() {
    let document = serde_json::to_value(
        HttpApi::new(Arc::new(MockHandler::default()))
            .with_authorizer(Arc::new(AllowAuthorizer))
            .build()
            .openapi,
    )
    .unwrap();

    for (path, method) in [
        ("/apis/solti.io/v1/tasks", "post"),
        ("/apis/solti.io/v1/tasks", "get"),
        ("/apis/solti.io/v1/tasks/{name}", "get"),
        ("/apis/solti.io/v1/tasks/{name}", "put"),
        ("/apis/solti.io/v1/tasks/{name}", "delete"),
        ("/apis/solti.io/v1/tasks/{name}/runs", "get"),
        ("/apis/solti.io/v1/tasks/{name}/logs", "get"),
    ] {
        assert!(
            document["paths"][path][method]["responses"]
                .get("403")
                .is_some(),
            "{method} {path} must document authorization denial"
        );
    }
}

#[derive(Clone)]
struct ApplicationState {
    revision: &'static str,
}

async fn application_route(
    axum::extract::State(state): axum::extract::State<ApplicationState>,
) -> axum::Json<TaskWorkload> {
    axum::Json(TaskWorkload::Embedded(
        EmbeddedSpec::new(state.revision).unwrap(),
    ))
}

async fn application_not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "application not found")
}

#[tokio::test]
async fn mount_preserves_application_state_schemas_and_runtime_perimeter() {
    let mut openapi = solti_api::aide::openapi::OpenApi::default();
    openapi.info.title = "Application API".into();
    openapi.security.push(
        [("applicationAuth".into(), Vec::new())]
            .into_iter()
            .collect(),
    );
    let app = solti_api::aide::axum::ApiRouter::<ApplicationState>::new()
        .api_route(
            "/application",
            solti_api::aide::axum::routing::get_with(application_route, |operation| {
                operation
                    .id("getApplication")
                    .response::<200, axum::Json<TaskWorkload>>()
            }),
        )
        .fallback(application_not_found);
    let app = HttpApi::new(Arc::new(MockHandler::default()))
        .with_auth(Token::new("task-secret").unwrap())
        .mount(app, &mut openapi)
        .with_state(ApplicationState {
            revision: "application-owned",
        });
    let router = app.finish_api(&mut openapi);
    let document = serde_json::to_value(&openapi).unwrap();

    assert_eq!(
        document["paths"]["/application"]["get"]["operationId"],
        "getApplication"
    );
    assert_eq!(document["info"]["title"], "Application API");
    assert_eq!(
        document["security"],
        serde_json::json!([{ "applicationAuth": [] }])
    );
    assert!(contains_const(
        &document["components"]["schemas"]["TaskWorkload"],
        "Embedded"
    ));
    assert!(!contains_const(
        &document["components"]["schemas"]["SoltiTaskWorkload"],
        "Embedded"
    ));
    assert!(
        document["paths"]["/application"]["get"]
            .get("security")
            .is_none()
    );
    assert_eq!(
        document["paths"]["/apis/solti.io/v1/tasks"]["post"]["security"],
        serde_json::json!([{ "soltiTaskBearer": [] }])
    );

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/application")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        serde_json::json!({
            "apiVersion": "solti.io/v1",
            "kind": "Embedded",
            "spec": {
                "revision": "application-owned"
            }
        })
    );

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/application-missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"application not found");

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/apis/solti.io/v1/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/apis/solti.io/v1/missing")
                .header("authorization", "Bearer task-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_status(&body_json(response).await, "NotFound", 404);
}

#[tokio::test]
async fn create_task_missing_required_resource_fields_returns_400() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/apis/solti.io/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn create_task_without_json_content_type_returns_status_415() {
    let handler = Arc::new(MockHandler::default());
    let response = router_with(Arc::clone(&handler))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/apis/solti.io/v1/tasks")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_status(
        &body_json(response).await,
        "UnsupportedMediaType",
        StatusCode::UNSUPPORTED_MEDIA_TYPE.as_u16(),
    );
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn create_task_malformed_json_returns_envelope() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/apis/solti.io/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from("{ not json at all"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/json"),
        "content-type must be JSON, got {ct:?}"
    );
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
    assert!(
        body["message"].is_string(),
        "message field must be a non-empty string, got {body:?}"
    );
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn create_task_oversize_body_returns_envelope_413() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let huge = "a".repeat(solti_api::MAX_REQUEST_BYTES + 1024);
    let body = format!(r#"{{"spec": "{huge}"}}"#);

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/apis/solti.io/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/json"),
        "413 must be JSON, got {ct:?}"
    );
    let body = body_json(resp).await;
    assert_status(&body, "RequestEntityTooLarge", 413);
    assert!(body["message"].as_str().unwrap().contains("exceeds"));
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn create_task_accepts_valid_json_between_axum_default_and_public_limit() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));
    let padding = "a".repeat(2 * 1024 * 1024 + 1024);
    let body = serde_json::json!({
        "apiVersion": "solti.io/v1",
        "kind": "Task",
        "metadata": {
            "name": "large-valid-task"
        },
        "spec": {
            "slot": "large-valid-task",
            "workload": {
                "apiVersion": "workloads.example.io/v1",
                "kind": "LargePayload",
                "spec": {
                    "padding": padding
                }
            },
            "timeout": 5000,
            "restart": { "type": "never" },
            "backoff": {
                "jitter": "full",
                "firstMs": 1000,
                "maxMs": 10000,
                "factor": 2.0
            },
            "admission": "dropIfRunning"
        }
    })
    .to_string();
    assert!(body.len() > 2 * 1024 * 1024);
    assert!(body.len() < solti_api::MAX_REQUEST_BYTES);

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/apis/solti.io/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_task_returns_404_when_absent() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_status(&body, "NotFound", 404);
}

#[tokio::test]
async fn router_level_errors_use_status_resources() {
    let cases = [
        (
            Method::GET,
            "/apis/solti.io/v1/tasks/%FF",
            StatusCode::BAD_REQUEST,
            "BadRequest",
        ),
        (
            Method::GET,
            "/apis/solti.io/v1/unknown",
            StatusCode::NOT_FOUND,
            "NotFound",
        ),
        (
            Method::GET,
            "/api/v1/tasks",
            StatusCode::NOT_FOUND,
            "NotFound",
        ),
        (
            Method::PATCH,
            "/apis/solti.io/v1/tasks",
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
        ),
    ];

    for (method, uri, expected_status, reason) in cases {
        let response = router_with(Arc::new(MockHandler::default()))
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), expected_status, "request {uri}");
        assert_status(&body_json(response).await, reason, expected_status.as_u16());
    }
}

#[tokio::test]
async fn list_runs_for_unknown_parent_returns_404() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/runs-missing/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_status(&body_json(resp).await, "NotFound", 404);
}

#[tokio::test]
async fn delete_unknown_task_returns_404_with_structured_error() {
    let handler = Arc::new(MockHandler {
        delete_returns_not_found: true,
        ..MockHandler::default()
    });
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/apis/solti.io/v1/tasks/missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_status(&body, "NotFound", 404);
    assert!(body["message"].as_str().unwrap().contains("missing"));
    assert_eq!(handler.delete_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delete_task_success_returns_204_no_content() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/apis/solti.io/v1/tasks/task-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(handler.delete_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn list_tasks_invalid_phase_returns_400() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?phase=totally_bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
    assert!(body["message"].as_str().unwrap().contains("invalid phase"));
}

#[tokio::test]
async fn list_tasks_invalid_pagination_returns_status_resource() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?limit=not-a-number")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_status(&body_json(resp).await, "BadRequest", 400);
}

#[tokio::test]
async fn list_tasks_rejects_limit_above_public_maximum() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?limit=1001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
    assert!(body["message"].as_str().unwrap().contains("1000"));
}

#[tokio::test]
async fn list_tasks_empty_returns_complete_collection_metadata() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["apiVersion"], "solti.io/v1");
    assert_eq!(body["kind"], "TaskList");
    assert_eq!(
        body["metadata"],
        serde_json::json!({ "resourceVersion": "test:1" })
    );
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_task_empty_name_trimmed_returns_400() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/%20%20")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("invalid task name")
    );
}

#[tokio::test]
async fn get_task_rejects_non_empty_name_outside_the_model_format() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/bad%24name")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
}

#[tokio::test]
async fn stream_task_logs_returns_sse_with_chunk_and_run_started_events() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(handler);

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/some-task/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "expected SSE content-type, got {ct}"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(
        body.contains("event: run-started"),
        "missing run-started in SSE body: {body}"
    );
    assert!(
        body.contains("event: chunk"),
        "missing chunk in SSE body: {body}"
    );
    assert!(
        body.contains("\"line\":\"aGVsbG8tZnJvbS1tb2Nr\""),
        "missing inlined chunk fields in SSE body: {body}"
    );
}

#[tokio::test]
async fn sse_metrics_span_response_body_until_eof() {
    let metrics = Arc::new(MetricsProbe::default());
    let app = router_with_metrics(Arc::new(MockHandler::default()), Arc::clone(&metrics));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/some-task/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 1);

    response.into_body().collect().await.unwrap();

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        metrics.last_status.load(Ordering::SeqCst),
        StatusCode::OK.as_u16()
    );
}

#[tokio::test]
async fn dropping_sse_body_releases_gauge_without_completion() {
    let metrics = Arc::new(MetricsProbe::default());
    let app = router_with_metrics(Arc::new(MockHandler::default()), Arc::clone(&metrics));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/stream-pending/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 1);

    drop(response);

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn watch_metrics_span_response_body_until_eof() {
    let metrics = Arc::new(MetricsProbe::default());
    let app = router_with_metrics(Arc::new(MockHandler::default()), Arc::clone(&metrics));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 0);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 1);

    response.into_body().collect().await.unwrap();

    assert_eq!(metrics.completed.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        metrics.last_status.load(Ordering::SeqCst),
        StatusCode::OK.as_u16()
    );
}

#[tokio::test]
async fn initial_sse_failure_is_recorded_immediately() {
    let metrics = Arc::new(MetricsProbe::default());
    let app = router_with_metrics(Arc::new(MockHandler::default()), Arc::clone(&metrics));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/stream-missing/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(metrics.completed.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        metrics.last_status.load(Ordering::SeqCst),
        StatusCode::NOT_FOUND.as_u16()
    );
}

#[tokio::test]
async fn stream_task_logs_missing_task_returns_404() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(handler);

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/stream-missing/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_status(&body, "NotFound", 404);
}

#[tokio::test]
async fn stream_task_logs_empty_id_returns_400() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/%20%20/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_status(&body, "BadRequest", 400);
}
