//! Auth (bearer-token) integration tests for the HTTP transport.
//!
//! Covers the perimeter installed by [`HttpApi::with_auth`]: every route —
//! including the SSE log stream — must reject requests without a valid
//! `Authorization: Bearer <token>` header before any handler work happens.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{ApiError, ApiHandler, HttpApi};
use solti_model::{Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec, Token};

const SECRET: &str = "sekret-token-1";

/// Minimal mock: succeeds at everything with empty fixtures and counts calls,
/// so tests can assert the auth gate rejected a request before any work ran.
#[derive(Default)]
struct MockHandler {
    calls: AtomicUsize,
}

#[async_trait]
impl ApiHandler for MockHandler {
    async fn submit_task(&self, _spec: TaskSpec) -> Result<TaskId, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TaskId::from("tsk_mock_1"))
    }

    async fn get_task_status(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TaskPage {
            items: Vec::new(),
            total: 0,
        })
    }

    async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn delete_task(&self, _id: &TaskId) -> Result<(), ApiError> {
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
    HttpApi::new(handler).with_auth(Token::new(SECRET)).router()
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

#[tokio::test]
async fn request_without_token_is_rejected_with_401() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app.oneshot(get("/api/v1/tasks")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "Unauthenticated");
    assert!(
        body["message"].as_str().unwrap().contains("bearer token"),
        "expected message to mention the bearer token, got {body:?}"
    );
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        0,
        "handler must not be reached without credentials"
    );
}

#[tokio::test]
async fn request_with_wrong_token_is_rejected_with_401() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app
        .oneshot(get_with_authorization(
            "/api/v1/tasks",
            "Bearer not-the-secret",
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "Unauthenticated");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn request_with_non_bearer_scheme_is_rejected_with_401() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app
        .oneshot(get_with_authorization(
            "/api/v1/tasks",
            &format!("Basic {SECRET}"),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn request_with_valid_token_reaches_the_handler() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app
        .oneshot(get_with_authorization(
            "/api/v1/tasks",
            &format!("Bearer {SECRET}"),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 0);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bearer_scheme_is_accepted_case_insensitively() {
    for scheme in ["bearer", "BEARER"] {
        let handler = Arc::new(MockHandler::default());
        let app = secured_router(Arc::clone(&handler));

        let resp = app
            .oneshot(get_with_authorization(
                "/api/v1/tasks",
                &format!("{scheme} {SECRET}"),
            ))
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "scheme {scheme:?} must be accepted"
        );
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn sse_logs_route_without_token_is_rejected_with_401() {
    let handler = Arc::new(MockHandler::default());
    let app = secured_router(Arc::clone(&handler));

    let resp = app.oneshot(get("/api/v1/tasks/tsk_1/logs")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "Unauthenticated");
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
            "/api/v1/tasks/tsk_1/logs",
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

#[test]
#[should_panic(expected = "auth token must not be empty")]
fn with_auth_panics_on_empty_token() {
    let _ = HttpApi::new(Arc::new(MockHandler::default())).with_auth(Token::new("   "));
}
