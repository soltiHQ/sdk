//! Integration tests for the HTTP transport.
//!
//! Exercise the axum `Router` end-to-end through `tower::ServiceExt::oneshot`:
//! real request parsing, route matching, validators, handler trait, error
//! conversion, status codes and JSON body shape. No TCP, no network.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{ApiError, ApiHandler, HttpApi};
use solti_model::{Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec};

/// Scriptable mock. `Default` succeeds at everything with harmless fixtures;
/// flip a flag to exercise the error branches.
#[derive(Default)]
struct MockHandler {
    submit_calls: AtomicUsize,
    cancel_calls: AtomicUsize,
    delete_calls: AtomicUsize,
    /// When true, `delete_task` returns `TaskNotFound`.
    delete_returns_not_found: bool,
}

#[async_trait]
impl ApiHandler for MockHandler {
    async fn submit_task(&self, _spec: TaskSpec) -> Result<TaskId, ApiError> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        Ok(TaskId::from("tsk_mock_1"))
    }

    async fn get_task_status(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(None)
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(TaskPage {
            items: Vec::new(),
            total: 0,
        })
    }

    async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        Ok(Vec::new())
    }

    async fn cancel_task(&self, _id: &TaskId) -> Result<(), ApiError> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.delete_returns_not_found {
            Err(ApiError::TaskNotFound(id.to_string()))
        } else {
            Ok(())
        }
    }
}

fn router_with(handler: Arc<MockHandler>) -> axum::Router {
    HttpApi::new(handler).router()
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body must be valid json")
}

#[tokio::test]
async fn submit_task_missing_spec_returns_400_with_structured_error() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "InvalidRequest");
    assert!(
        body["message"].as_str().unwrap().contains("missing spec"),
        "expected message to mention 'missing spec', got {body:?}"
    );
    assert_eq!(handler.submit_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn get_task_status_omits_task_field_when_absent() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks/unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // canonical proto3-JSON: `optional message` fields are OMITTED when None
    // (not `null`). `.emit_fields()` only toggles scalar/repeated defaults.
    assert!(
        body.get("task").is_none(),
        "expected `task` field absent, got {body:?}"
    );
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
                .uri("/api/v1/tasks/missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "TaskNotFound");
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
                .uri("/api/v1/tasks/tsk_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(handler.delete_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_task_success_returns_204_no_content() {
    let handler = Arc::new(MockHandler::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tasks/tsk_1/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(handler.cancel_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn list_tasks_invalid_status_returns_400() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks?status=totally_bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "InvalidRequest");
    assert!(body["message"].as_str().unwrap().contains("invalid status"));
}

#[tokio::test]
async fn list_tasks_empty_returns_empty_list_and_zero_total() {
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 0);
    assert!(body["tasks"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn request_body_limit_rejects_oversized_payload() {
    let app = router_with(Arc::new(MockHandler::default()));

    // Router applies RequestBodyLimitLayer = 256 KiB. Send a 300 KiB blob.
    let oversized = vec![b'x'; 300 * 1024];
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(oversized))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn get_task_status_empty_id_trimmed_returns_400() {
    // `/api/v1/tasks/   ` after url-decoding is whitespace-only; our
    // non_empty_id validator should catch it.
    let app = router_with(Arc::new(MockHandler::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks/%20%20")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "InvalidRequest");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("task_id cannot be empty")
    );
}
