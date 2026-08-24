//! Auth (bearer-token) integration tests for the HTTP transport.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{
    ApiAuthenticator, ApiAuthorizer, ApiError, ApiHandler, ApiIdentity, AuthenticationRequest,
    AuthorizationRequest, HttpApi, TaskOperation, TaskTarget, TaskWatchEventStream, Transport,
};
use solti_model::{
    Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRunPage, TaskRunQuery, Token,
    Uid, WritePreconditions,
};

const SECRET: &str = "sekret-token-1";

#[derive(Default)]
struct MockHandler {
    calls: AtomicUsize,
}

#[async_trait]
impl ApiHandler for MockHandler {
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
    }

    async fn apply_task(
        &self,
        manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
    }

    async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(tokio_stream::empty()))
    }

    async fn query_task_runs(
        &self,
        id: &TaskId,
        _query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TaskRunPage {
            items: Vec::new(),
            task: id.clone(),
            task_uid: Uid::new("auth-http-run-uid").unwrap(),
            resource_version: "runs-test:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn cancel_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stream_task_logs(
        &self,
        _id: &TaskId,
    ) -> Result<solti_api::OutputEventStream, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(tokio_stream::empty()))
    }
}

fn secured_router(handler: Arc<MockHandler>) -> axum::Router {
    HttpApi::new(handler)
        .with_auth(Token::new(SECRET).unwrap())
        .router()
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body must be valid json")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn get_with_authorization(uri: &str, value: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, value)
        .body(Body::empty())
        .unwrap()
}

fn post_with_authorization(uri: &str, value: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, value)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn invalid_credentials_are_rejected_before_the_handler() {
    for authorization in [
        None,
        Some("Bearer not-the-secret"),
        Some("Basic sekret-token-1"),
    ] {
        let handler = Arc::new(MockHandler::default());
        let app = secured_router(Arc::clone(&handler));
        let request = match authorization {
            Some(value) => get_with_authorization("/apis/solti.io/v1/tasks", value),
            None => get("/apis/solti.io/v1/tasks"),
        };

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        let body = body_json(response).await;
        assert_eq!(body["apiVersion"], "v1");
        assert_eq!(body["kind"], "Status");
        assert_eq!(body["metadata"], serde_json::json!({}));
        assert_eq!(body["status"], "Failure");
        assert_eq!(body["reason"], "Unauthorized");
        assert_eq!(body["code"], 401);
        assert!(
            body["message"].as_str().unwrap().contains("bearer token"),
            "expected message to mention the bearer token, got {body:?}"
        );
        assert_eq!(
            handler.calls.load(Ordering::SeqCst),
            0,
            "handler must not be reached with {authorization:?}"
        );
    }
}

#[tokio::test]
async fn valid_bearer_schemes_reach_the_handler() {
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let handler = Arc::new(MockHandler::default());
        let app = secured_router(Arc::clone(&handler));

        let resp = app
            .oneshot(get_with_authorization(
                "/apis/solti.io/v1/tasks",
                &format!("{scheme} {SECRET}"),
            ))
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
        assert_eq!(body["items"], serde_json::json!([]));
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn sse_logs_route_without_token_is_rejected_with_401() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app
        .oneshot(get("/apis/solti.io/v1/tasks/task-1/logs"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(resp.headers()[header::WWW_AUTHENTICATE], "Bearer");
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "Unauthorized");
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        0,
        "no log subscription may be created without credentials"
    );
}

#[tokio::test]
async fn sse_logs_route_with_valid_token_streams() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app
        .oneshot(get_with_authorization(
            "/apis/solti.io/v1/tasks/task-1/logs",
            &format!("Bearer {SECRET}"),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.starts_with("text/event-stream"),
        "expected SSE content-type, got {ct:?}"
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

struct SubjectAuthenticator;

#[async_trait]
impl ApiAuthenticator for SubjectAuthenticator {
    async fn authenticate(
        &self,
        request: AuthenticationRequest<'_>,
    ) -> Result<ApiIdentity, ApiError> {
        if request.transport() == Transport::Http
            && request.bearer_credential() == Some("subject-token")
        {
            Ok(ApiIdentity::for_subject("user-7").with_attribute("team", "runtime"))
        } else {
            Err(ApiError::Unauthenticated("credential rejected".into()))
        }
    }
}

#[derive(Default)]
struct RecordingAuthorizer {
    checks: std::sync::Mutex<Vec<(Option<String>, TaskOperation, String)>>,
}

#[async_trait]
impl ApiAuthorizer for RecordingAuthorizer {
    async fn authorize(&self, request: AuthorizationRequest<'_>) -> Result<(), ApiError> {
        let target = match request.target() {
            TaskTarget::Collection => "collection".to_owned(),
            TaskTarget::Task(task) => task.to_string(),
            TaskTarget::Manifest(manifest) => manifest.name().to_string(),
            _ => "unknown".to_owned(),
        };
        self.checks.lock().unwrap().push((
            request
                .identity()
                .and_then(ApiIdentity::subject)
                .map(str::to_owned),
            request.operation(),
            target,
        ));
        if request.operation() == TaskOperation::StreamLogs {
            Err(ApiError::Forbidden("log access denied".into()))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn custom_access_hooks_propagate_identity_and_return_forbidden() {
    let handler = Arc::new(MockHandler::default());
    let recording = Arc::new(RecordingAuthorizer::default());
    let app = HttpApi::new(Arc::clone(&handler))
        .with_authenticator(Arc::new(SubjectAuthenticator))
        .with_authorizer(recording.clone())
        .router();

    let listed = app
        .clone()
        .oneshot(get_with_authorization(
            "/apis/solti.io/v1/tasks",
            "Bearer subject-token",
        ))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let canceled = app
        .clone()
        .oneshot(post_with_authorization(
            "/apis/solti.io/v1/tasks/task-a/cancel",
            "Bearer subject-token",
        ))
        .await
        .unwrap();
    assert_eq!(canceled.status(), StatusCode::NO_CONTENT);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);

    let denied = app
        .oneshot(get_with_authorization(
            "/apis/solti.io/v1/tasks/task-a/logs",
            "Bearer subject-token",
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let body = body_json(denied).await;
    assert_eq!(body["reason"], "Forbidden");
    assert_eq!(body["code"], 403);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);

    assert_eq!(
        *recording.checks.lock().unwrap(),
        vec![
            (
                Some("user-7".to_owned()),
                TaskOperation::List,
                "collection".to_owned(),
            ),
            (
                Some("user-7".to_owned()),
                TaskOperation::Cancel,
                "task-a".to_owned(),
            ),
            (
                Some("user-7".to_owned()),
                TaskOperation::StreamLogs,
                "task-a".to_owned(),
            ),
        ]
    );
}
