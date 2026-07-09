//! Wire-shape pinning tests for the HTTP transport.
//!
//! Every request body documented in `api_v1.md` is replayed against the real
//! router (which deserializes it into the generated proto types via pbjson),
//! and every documented response shape is asserted key-by-key. If these tests
//! fail, either the wire contract or `api_v1.md` drifted — fix whichever lies.

#![cfg(feature = "http")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{ApiError, ApiHandler, HttpApi, OutputEventStream};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, Flag, JitterPolicy, OutputChunk, OutputEvent, RestartPolicy,
    StreamKind, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskKind, TaskPage,
    TaskPhase, TaskQuery, TaskRun, TaskSpec, Token,
};

// ---------------------------------------------------------------------------
// Request bodies — verbatim copies of the `api_v1.md` HTTP examples.
// ---------------------------------------------------------------------------

const SUBMIT_COMMAND_BODY: &str = r#"{
  "spec": {
    "slot": "my-job",
    "kind": {
      "subprocess": {
        "command": {
          "command": "echo",
          "args": ["hello world"]
        },
        "env": [],
        "failOnNonZero": true
      }
    },
    "timeoutMs": "30000",
    "restart": "RESTART_POLICY_NEVER",
    "backoff": {
      "jitter": "JITTER_POLICY_FULL",
      "firstMs": "1000",
      "maxMs": "10000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
  }
}"#;

const SUBMIT_SCRIPT_BODY: &str = r#"{
  "spec": {
    "slot": "my-script",
    "kind": {
      "subprocess": {
        "script": {
          "wellKnown": "SCRIPT_RUNTIME_BASH",
          "body": "ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==",
          "args": []
        },
        "env": [
          { "key": "ENV", "value": "production" }
        ],
        "failOnNonZero": true
      }
    },
    "timeoutMs": "60000",
    "restart": "RESTART_POLICY_ON_FAILURE",
    "backoff": {
      "jitter": "JITTER_POLICY_EQUAL",
      "firstMs": "2000",
      "maxMs": "30000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_REPLACE"
  }
}"#;

/// The "custom runtime" fragment from the docs, wrapped in the same spec envelope.
const SUBMIT_CUSTOM_RUNTIME_BODY: &str = r#"{
  "spec": {
    "slot": "my-script",
    "kind": {
      "subprocess": {
        "script": {
          "custom": { "command": "ruby", "flag": "-e" },
          "body": "cHV0cyAnaGVsbG8n",
          "args": []
        },
        "failOnNonZero": true
      }
    },
    "timeoutMs": "30000",
    "restart": "RESTART_POLICY_NEVER",
    "backoff": {
      "jitter": "JITTER_POLICY_FULL",
      "firstMs": "1000",
      "maxMs": "10000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
  }
}"#;

const APPLY_BODY: &str = r#"{
  "spec": {
    "slot": "my-job",
    "kind": {
      "subprocess": {
        "command": { "command": "echo", "args": ["v2"] },
        "failOnNonZero": true
      }
    },
    "timeoutMs": "30000",
    "restart": "RESTART_POLICY_NEVER",
    "backoff": {
      "jitter": "JITTER_POLICY_FULL",
      "firstMs": "1000",
      "maxMs": "10000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
  }
}"#;

// ---------------------------------------------------------------------------
// Mock handler with fixtures matching the documented response examples.
// ---------------------------------------------------------------------------

/// Task fixture mirroring the "Get task status" example in `api_v1.md`.
fn fixture_task() -> Task {
    let kind = TaskKind::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello world".into()],
        },
        TaskEnv::new(),
        None,
        Flag::from(true),
    ));
    let spec = TaskSpec::builder("my-job", kind, 30_000_u64)
        .restart(RestartPolicy::Never)
        .backoff(BackoffPolicy {
            jitter: JitterPolicy::Full,
            first_ms: 1_000,
            max_ms: 10_000,
            factor: 2.0,
        })
        .admission(AdmissionPolicy::DropIfRunning)
        .build()
        .expect("fixture spec must be valid");

    let mut task = Task::new(TaskId::from("tsk_wire_1"), spec);
    task.transition_starting();
    task.transition_finished(TaskPhase::Succeeded, None, Some(0))
        .expect("fixture transition must be valid");
    task
}

/// Run fixtures mirroring the "List task runs" example in `api_v1.md`.
fn fixture_runs() -> Vec<TaskRun> {
    let mut failed = TaskRun::starting(1);
    failed.started_at = UNIX_EPOCH + Duration::from_millis(1_712_750_400_000);
    failed.finished_at = Some(UNIX_EPOCH + Duration::from_millis(1_712_750_402_000));
    failed.phase = TaskPhase::Failed;
    failed.error = Some("exit code 1".into());
    failed.exit_code = Some(1);

    let mut succeeded = TaskRun::starting(2);
    succeeded.started_at = UNIX_EPOCH + Duration::from_millis(1_712_750_405_000);
    succeeded.finished_at = Some(UNIX_EPOCH + Duration::from_millis(1_712_750_406_000));
    succeeded.phase = TaskPhase::Succeeded;
    succeeded.exit_code = Some(0);

    vec![failed, succeeded]
}

#[derive(Default)]
struct WireMock {
    last_admission: Mutex<Option<AdmissionPolicy>>,
    submit_conflicts: bool,
}

#[async_trait]
impl ApiHandler for WireMock {
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError> {
        *self.last_admission.lock().unwrap() = Some(spec.admission());
        if self.submit_conflicts {
            return Err(ApiError::Core(solti_core::CoreError::AlreadyExists(
                "my-job".into(),
            )));
        }
        Ok(TaskId::from("tsk_wire_1"))
    }

    async fn get_task_status(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(Some(fixture_task()))
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(TaskPage {
            items: vec![fixture_task()],
            total: 1,
        })
    }

    async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        Ok(fixture_runs())
    }

    async fn delete_task(&self, _id: &TaskId) -> Result<(), ApiError> {
        Ok(())
    }

    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        // Fixtures mirroring the SSE frames documented in `api_v1.md`.
        let events = vec![
            OutputEvent::RunStarted {
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_712_750_400_000),
            },
            OutputEvent::Chunk(OutputChunk {
                attempt: 1,
                stream: StreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_712_750_400_123),
                line: Bytes::from_static(b"hello world"),
            }),
            OutputEvent::RunFinished {
                attempt: 1,
                exit_code: Some(0),
                finished_at: UNIX_EPOCH + Duration::from_millis(1_712_750_400_456),
            },
            OutputEvent::Lagged { skipped: 42 },
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn router_with(handler: Arc<WireMock>) -> axum::Router {
    HttpApi::new(handler).router()
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response body must be valid json")
}

fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Documented request bodies must deserialize into the proto types.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_command_body_from_docs_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/api/v1/tasks", SUBMIT_COMMAND_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["taskId"], "tsk_wire_1");
    assert!(
        body.get("task_id").is_none(),
        "response must use camelCase taskId, got {body:?}"
    );
}

#[tokio::test]
async fn submit_script_body_from_docs_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/api/v1/tasks", SUBMIT_SCRIPT_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert!(body["taskId"].is_string(), "expected taskId in {body:?}");
}

#[tokio::test]
async fn submit_custom_runtime_body_from_docs_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/api/v1/tasks", SUBMIT_CUSTOM_RUNTIME_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn submit_accepts_numeric_timeout_ms_on_input() {
    // The docs promise: 64-bit integers are canonically strings, but plain
    // JSON numbers are also accepted on input.
    let app = router_with(Arc::new(WireMock::default()));
    let body = SUBMIT_COMMAND_BODY.replace(r#""timeoutMs": "30000""#, r#""timeoutMs": 30000"#);
    assert_ne!(body, SUBMIT_COMMAND_BODY, "replacement must hit");

    let resp = app
        .oneshot(post_json("/api/v1/tasks", &body))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn submit_rejects_legacy_domain_shaped_body() {
    // The pre-pbjson domain shape (`timeout`, tagged `restart`, `mode` wrapper)
    // is NOT the wire contract; unknown fields must be rejected with 400.
    let app = router_with(Arc::new(WireMock::default()));
    let legacy = r#"{
      "spec": {
        "slot": "my-job",
        "kind": {
          "subprocess": {
            "mode": { "command": { "command": "echo", "args": [] } },
            "failOnNonZero": true
          }
        },
        "timeout": 30000,
        "restart": { "type": "never" },
        "backoff": { "jitter": "full", "firstMs": 1000, "maxMs": 10000, "factor": 2.0 },
        "admission": "dropIfRunning"
      }
    }"#;

    let resp = app
        .oneshot(post_json("/api/v1/tasks", legacy))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "InvalidRequest");
}

#[tokio::test]
async fn apply_body_from_docs_returns_200_and_forces_replace() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/v1/tasks")
                .header("content-type", "application/json")
                .body(Body::from(APPLY_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["taskId"], "tsk_wire_1");

    // The documented spec declares DROP_IF_RUNNING, yet apply must force Replace.
    assert_eq!(
        *handler.last_admission.lock().unwrap(),
        Some(AdmissionPolicy::Replace),
        "apply must override the spec's admission with Replace"
    );
}

// ---------------------------------------------------------------------------
// Documented response shapes must match what the transport actually emits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_task_status_response_matches_documented_shape() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks/tsk_wire_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let task = &body["task"];

    let meta = &task["metadata"];
    assert_eq!(meta["id"], "tsk_wire_1");
    assert!(
        meta["createdAt"].is_string() && meta["updatedAt"].is_string(),
        "timestamps must be proto-JSON strings, got {meta:?}"
    );
    assert!(
        meta["resourceVersion"].is_string(),
        "resourceVersion (uint64) must be a string, got {meta:?}"
    );

    let spec = &task["spec"];
    assert_eq!(spec["slot"], "my-job");
    assert_eq!(
        spec["timeoutMs"], "30000",
        "timeoutMs (uint64) must be a string, got {spec:?}"
    );
    assert_eq!(spec["restart"], "RESTART_POLICY_NEVER");
    assert_eq!(spec["admission"], "ADMISSION_POLICY_DROP_IF_RUNNING");
    assert!(spec["labels"].is_object(), "labels are always emitted");

    let backoff = &spec["backoff"];
    assert_eq!(backoff["jitter"], "JITTER_POLICY_FULL");
    assert_eq!(backoff["firstMs"], "1000");
    assert_eq!(backoff["maxMs"], "10000");
    assert_eq!(backoff["factor"], 2.0);

    // Both oneofs (`kind`, subprocess `mode`) arrive flattened.
    let sub = &spec["kind"]["subprocess"];
    assert_eq!(sub["command"]["command"], "echo");
    assert_eq!(sub["command"]["args"][0], "hello world");
    assert_eq!(sub["failOnNonZero"], true);
    assert!(sub["env"].is_array(), "env is always emitted");
    assert!(
        sub.get("mode").is_none(),
        "oneof must be flattened: no `mode` wrapper, got {sub:?}"
    );

    let status = &task["status"];
    assert_eq!(status["phase"], "TASK_PHASE_SUCCEEDED");
    assert_eq!(status["attempt"], 1);
    assert_eq!(status["exitCode"], 0);
}

#[tokio::test]
async fn list_tasks_response_matches_documented_shape() {
    let app = router_with(Arc::new(WireMock::default()));

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
    assert_eq!(body["total"], 1, "total (uint32) is a plain number");
    assert_eq!(body["tasks"][0]["metadata"]["id"], "tsk_wire_1");
    assert_eq!(body["tasks"][0]["status"]["phase"], "TASK_PHASE_SUCCEEDED");
}

#[tokio::test]
async fn list_tasks_phase_filter_accepts_documented_values() {
    for phase in [
        "pending",
        "running",
        "succeeded",
        "failed",
        "timeout",
        "canceled",
        "exhausted",
    ] {
        let app = router_with(Arc::new(WireMock::default()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/v1/tasks?phase={phase}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "documented phase value '{phase}' must be accepted"
        );
    }
}

#[tokio::test]
async fn list_task_runs_response_matches_documented_shape() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks/tsk_wire_1/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let runs = body["runs"].as_array().expect("runs must be an array");
    assert_eq!(runs.len(), 2);

    assert_eq!(runs[0]["attempt"], 1);
    assert_eq!(runs[0]["phase"], "TASK_PHASE_FAILED");
    assert_eq!(
        runs[0]["startedAt"], "1712750400000",
        "startedAt (int64) must be a string"
    );
    assert_eq!(runs[0]["finishedAt"], "1712750402000");
    assert_eq!(runs[0]["error"], "exit code 1");
    assert_eq!(runs[0]["exitCode"], 1, "exitCode (int32) is a plain number");

    assert_eq!(runs[1]["attempt"], 2);
    assert_eq!(runs[1]["phase"], "TASK_PHASE_SUCCEEDED");
    assert_eq!(runs[1]["startedAt"], "1712750405000");
    assert_eq!(runs[1]["finishedAt"], "1712750406000");
    assert_eq!(runs[1]["exitCode"], 0);
}

#[tokio::test]
async fn sse_frames_match_documented_event_names_and_payloads() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks/tsk_wire_1/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&bytes).unwrap();

    // Frames exactly as documented in the SSE section of api_v1.md: the
    // payload is the domain OutputEvent encoding, not proto-JSON.
    for frame in [
        "event: run-started\ndata: {\"type\":\"runStarted\",\"attempt\":1,\"startedAt\":1712750400000}",
        "event: chunk\ndata: {\"type\":\"chunk\",\"attempt\":1,\"stream\":\"stdout\",\"seq\":0,\"ts\":1712750400123,\"line\":\"hello world\"}",
        "event: run-finished\ndata: {\"type\":\"runFinished\",\"attempt\":1,\"exitCode\":0,\"finishedAt\":1712750400456}",
        "event: lagged\ndata: {\"type\":\"lagged\",\"skipped\":42}",
    ] {
        assert!(
            body.contains(frame),
            "missing documented SSE frame:\n{frame}\nin body:\n{body}"
        );
    }
}

// ---------------------------------------------------------------------------
// Documented error mappings.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slot_conflict_maps_to_409_already_exists() {
    let handler = Arc::new(WireMock {
        submit_conflicts: true,
        ..WireMock::default()
    });
    let app = router_with(handler);

    let resp = app
        .oneshot(post_json("/api/v1/tasks", SUBMIT_COMMAND_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "AlreadyExists");
}

#[tokio::test]
async fn missing_bearer_token_maps_to_401_unauthenticated() {
    let app = HttpApi::new(Arc::new(WireMock::default()))
        .with_auth(Token::new("secret-token"))
        .router();

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

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "Unauthenticated");
}

#[tokio::test]
async fn valid_bearer_token_passes_the_auth_gate() {
    let app = HttpApi::new(Arc::new(WireMock::default()))
        .with_auth(Token::new("secret-token"))
        .router();

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/tasks")
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}
