//! Wire-shape pinning tests for the HTTP transport.
//!
//! The tests replay CRD-shaped resources through the real router and pin the
//! model-owned JSON contract key by key.

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
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, OutputChunk, OutputEvent,
    RestartPolicy, StreamKind, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskManifest,
    TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec, TaskWorkload, Token, WorkloadTypeMeta,
};

// ---------------------------------------------------------------------------
// CRD-shaped request bodies.
// ---------------------------------------------------------------------------

const CREATE_COMMAND_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "task-wire-1" },
  "spec": {
    "slot": "my-job",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "command": {
            "command": "echo",
            "args": ["hello world"]
          }
        },
        "env": [],
        "failOnNonZero": true
      }
    },
    "timeout": 30000,
    "restart": { "type": "never" },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 10000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}"#;

const CREATE_SCRIPT_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "task-script-1" },
  "spec": {
    "slot": "my-script",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "script": {
            "runtime": "bash",
            "body": "ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==",
            "args": []
          }
        },
        "env": [
          { "key": "ENV", "value": "production" }
        ],
        "failOnNonZero": true
      }
    },
    "timeout": 60000,
    "restart": { "type": "onFailure" },
    "backoff": {
      "jitter": "equal",
      "firstMs": 2000,
      "maxMs": 30000,
      "factor": 2.0
    },
    "admission": "replace"
  }
}"#;

/// The "custom runtime" fragment from the docs, wrapped in the same spec envelope.
const CREATE_CUSTOM_RUNTIME_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "task-ruby-1" },
  "spec": {
    "slot": "my-script",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "script": {
            "runtime": {
              "custom": { "command": "ruby", "flag": "-e" }
            },
            "body": "cHV0cyAnaGVsbG8n",
            "args": []
          }
        },
        "failOnNonZero": true
      }
    },
    "timeout": 30000,
    "restart": { "type": "never" },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 10000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}"#;

const APPLY_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "task-wire-1" },
  "spec": {
    "slot": "my-job",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "command": { "command": "echo", "args": ["v2"] }
        },
        "failOnNonZero": true
      }
    },
    "timeout": 30000,
    "restart": { "type": "never" },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 10000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}"#;

const CREATE_EXTENSION_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "task-extension-1" },
  "spec": {
    "slot": "custom-job",
    "workload": {
      "apiVersion": "example.io/v1",
      "kind": "Snapshot",
      "spec": {
        "bucket": "reports",
        "compress": true,
        "exactInteger": 9007199254740993
      }
    },
    "timeout": 30000,
    "restart": { "type": "never" },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 10000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}"#;

const CREATE_EMBEDDED_BODY: &str = r#"{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": { "name": "embedded-task" },
  "spec": {
    "slot": "internal",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Embedded",
      "spec": { "revision": "test-v1" }
    },
    "timeout": 1000,
    "restart": { "type": "never" },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 10000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}"#;

// ---------------------------------------------------------------------------
// Mock handler with fixtures matching the documented response examples.
// ---------------------------------------------------------------------------

/// Task fixture mirroring the "Get task status" example in `api_v1.md`.
fn fixture_task() -> Task {
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello world".into()],
        },
        TaskEnv::new(),
        None,
        Flag::from(true),
    ));
    let spec = TaskSpec::builder("my-job", workload, 30_000_u64)
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

    let mut task = Task::new("task-wire-1", spec).expect("fixture task must be valid");
    task.set_resource_version("3").unwrap();
    task.transition_starting(1, 1, "4").unwrap();
    task.transition_finished(1, 1, TaskPhase::Succeeded, None, Some(0), "5")
        .expect("fixture transition must be valid");
    task
}

fn embedded_task() -> Task {
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap());
    let spec = TaskSpec::builder("internal", workload, 1_000_u64)
        .build()
        .unwrap();
    Task::new("embedded-task", spec).unwrap()
}

/// Run fixtures mirroring the "List task runs" example in `api_v1.md`.
fn fixture_runs() -> Vec<TaskRun> {
    let workload = WorkloadTypeMeta::new("solti.io/v1", "Subprocess").unwrap();
    let mut failed = TaskRun::starting(1, 1, workload.clone());
    failed.started_at = UNIX_EPOCH + Duration::from_millis(1_712_750_400_000);
    failed.finished_at = Some(UNIX_EPOCH + Duration::from_millis(1_712_750_402_000));
    failed.phase = TaskPhase::Failed;
    failed.error = Some("exit code 1".into());
    failed.exit_code = Some(1);

    let mut succeeded = TaskRun::starting(1, 2, workload);
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
    leak_embedded_task: bool,
    leak_embedded_run: bool,
}

#[async_trait]
impl ApiHandler for WireMock {
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        *self.last_admission.lock().unwrap() = Some(manifest.spec().admission());
        if self.submit_conflicts {
            return Err(ApiError::AlreadyExists("my-job".into()));
        }
        Ok(if self.leak_embedded_task {
            embedded_task()
        } else {
            Task::from_manifest(manifest).unwrap()
        })
    }

    async fn apply_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        *self.last_admission.lock().unwrap() = Some(manifest.spec().admission());
        Ok(if self.leak_embedded_task {
            embedded_task()
        } else {
            Task::from_manifest(manifest).unwrap()
        })
    }

    async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(Some(if self.leak_embedded_task {
            embedded_task()
        } else {
            fixture_task()
        }))
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(TaskPage {
            items: vec![if self.leak_embedded_task {
                embedded_task()
            } else {
                fixture_task()
            }],
            total: 3,
        })
    }

    async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        if self.leak_embedded_run {
            return Ok(vec![TaskRun::starting(
                1,
                1,
                WorkloadTypeMeta::new("solti.io/v1", "Embedded").unwrap(),
            )]);
        }
        Ok(fixture_runs())
    }

    async fn delete_task(&self, _id: &TaskId) -> Result<(), ApiError> {
        Ok(())
    }

    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        // Fixtures mirroring the SSE frames documented in `api_v1.md`.
        let events = vec![
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_712_750_400_000),
            },
            OutputEvent::Chunk(OutputChunk {
                generation: 1,
                attempt: 1,
                stream: StreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_712_750_400_123),
                line: Bytes::from_static(b"hello world"),
            }),
            OutputEvent::RunFinished {
                generation: 1,
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
// CRD request bodies must deserialize through solti-model.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_command_resource_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", CREATE_COMMAND_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["metadata"]["name"], "task-wire-1");
    assert_eq!(body["apiVersion"], "solti.io/v1");
    assert_eq!(body["kind"], "Task");
}

#[tokio::test]
async fn create_script_resource_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", CREATE_SCRIPT_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["metadata"]["name"], "task-script-1");
}

#[tokio::test]
async fn create_custom_runtime_resource_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json(
            "/apis/solti.io/v1/tasks",
            CREATE_CUSTOM_RUNTIME_BODY,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_extension_workload_is_accepted_and_returned_unchanged() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", CREATE_EXTENSION_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["spec"]["workload"]["apiVersion"], "example.io/v1");
    assert_eq!(body["spec"]["workload"]["kind"], "Snapshot");
    assert_eq!(
        body["spec"]["workload"]["spec"],
        serde_json::json!({
            "bucket": "reports",
            "compress": true,
            "exactInteger": 9_007_199_254_740_993_u64
        })
    );
}

#[tokio::test]
async fn create_embedded_workload_is_rejected_before_the_handler() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", CREATE_EMBEDDED_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "BadRequest");
    assert_eq!(*handler.last_admission.lock().unwrap(), None);
}

#[tokio::test]
async fn create_rejects_proto_json_string_timeout() {
    let app = router_with(Arc::new(WireMock::default()));
    let body = CREATE_COMMAND_BODY.replace(r#""timeout": 30000"#, r#""timeout": "30000""#);
    assert_ne!(body, CREATE_COMMAND_BODY, "replacement must hit");

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", &body))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["reason"], "BadRequest");
}

#[tokio::test]
async fn create_rejects_removed_task_spec_fields() {
    // Removed TaskSpec fields (`kind`, `timeout`) are not part of the current
    // CRD resource contract and must be rejected.
    let app = router_with(Arc::new(WireMock::default()));
    let legacy = r#"{
      "apiVersion": "solti.io/v1",
      "kind": "Task",
      "metadata": { "name": "legacy-task" },
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
        .oneshot(post_json("/apis/solti.io/v1/tasks", legacy))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "BadRequest");
}

#[tokio::test]
async fn create_rejects_workload_kind_and_spec_mismatch() {
    let app = router_with(Arc::new(WireMock::default()));
    let body = CREATE_COMMAND_BODY.replacen(r#""kind": "Subprocess""#, r#""kind": "Container""#, 1);
    assert_ne!(body, CREATE_COMMAND_BODY, "replacement must hit");

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", &body))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "BadRequest");
}

#[tokio::test]
async fn apply_resource_returns_200_and_preserves_admission_policy() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/apis/solti.io/v1/tasks/task-wire-1")
                .header("content-type", "application/json")
                .body(Body::from(APPLY_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["metadata"]["name"], "task-wire-1");

    assert_eq!(
        *handler.last_admission.lock().unwrap(),
        Some(AdmissionPolicy::DropIfRunning),
        "apply must preserve the desired admission policy"
    );
}

#[tokio::test]
async fn apply_rejects_path_and_metadata_name_mismatch() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/apis/solti.io/v1/tasks/another-task")
                .header("content-type", "application/json")
                .body(Body::from(APPLY_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["reason"], "BadRequest");
    assert_eq!(*handler.last_admission.lock().unwrap(), None);
}

// ---------------------------------------------------------------------------
// Documented response shapes must match what the transport actually emits.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_task_response_matches_crd_shape() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/task-wire-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let task = &body;
    assert_eq!(task["apiVersion"], "solti.io/v1");
    assert_eq!(task["kind"], "Task");

    let meta = &task["metadata"];
    assert_eq!(meta["name"], "task-wire-1");
    assert!(
        meta["creationTimestamp"].is_string(),
        "Kubernetes resource timestamps must use RFC 3339"
    );
    assert!(
        meta["resourceVersion"].is_string(),
        "resourceVersion must be an opaque string, got {meta:?}"
    );
    assert_eq!(meta["generation"], 1);
    assert!(meta["uid"].is_string());

    let spec = &task["spec"];
    assert_eq!(spec["slot"], "my-job");
    assert_eq!(spec["timeout"], 30000);
    assert_eq!(spec["restart"], serde_json::json!({ "type": "never" }));
    assert_eq!(spec["admission"], "dropIfRunning");

    let backoff = &spec["backoff"];
    assert_eq!(backoff["jitter"], "full");
    assert_eq!(backoff["firstMs"], 1000);
    assert_eq!(backoff["maxMs"], 10000);
    assert_eq!(backoff["factor"], 2.0);

    let workload = &spec["workload"];
    assert_eq!(workload["apiVersion"], "solti.io/v1");
    assert_eq!(workload["kind"], "Subprocess");
    let sub = &workload["spec"];
    assert_eq!(sub["mode"]["command"]["command"], "echo");
    assert_eq!(sub["mode"]["command"]["args"][0], "hello world");
    assert_eq!(sub["failOnNonZero"], true);
    assert!(
        sub.get("env").is_none(),
        "empty env must be omitted exactly as in solti-model serde: {sub:?}"
    );
    assert!(
        sub.get("subprocess").is_none(),
        "workload spec must be direct: no `subprocess` discriminator, got {sub:?}"
    );

    let status = &task["status"];
    assert_eq!(status["observedGeneration"], 1);
    assert_eq!(status["phase"], "succeeded");
    assert_eq!(status["attempt"], 1);
    assert_eq!(status["exitCode"], 0);
    let conditions = status["conditions"].as_array().expect("conditions array");
    assert_eq!(conditions.len(), 1);
    let reconciled = &conditions[0];
    assert_eq!(reconciled["type"], "Reconciled");
    assert_eq!(reconciled["status"], "True");
    assert_eq!(reconciled["observedGeneration"], 1);
    assert_eq!(reconciled["reason"], "RuntimeAccepted");
    assert_eq!(
        reconciled["message"],
        "Taskvisor accepted the runtime realization"
    );
    assert!(reconciled["lastTransitionTime"].is_string());
}

#[tokio::test]
async fn get_rejects_embedded_task_leaked_by_custom_handler() {
    let app = router_with(Arc::new(WireMock {
        leak_embedded_task: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/embedded-task")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(resp).await["reason"], "InternalError");
}

#[tokio::test]
async fn list_tasks_response_matches_documented_shape() {
    let app = router_with(Arc::new(WireMock::default()));

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
    assert_eq!(body["metadata"]["remainingItemCount"], 2);
    assert_eq!(body["items"][0]["metadata"]["name"], "task-wire-1");
    assert_eq!(body["items"][0]["status"]["phase"], "succeeded");
}

#[tokio::test]
async fn list_rejects_embedded_task_leaked_by_custom_handler() {
    let app = router_with(Arc::new(WireMock {
        leak_embedded_task: true,
        ..WireMock::default()
    }));

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

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(resp).await["reason"], "InternalError");
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
                    .uri(format!("/apis/solti.io/v1/tasks?phase={phase}"))
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
                .uri("/apis/solti.io/v1/tasks/task-wire-1/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let runs = body["runs"].as_array().expect("runs must be an array");
    assert_eq!(runs.len(), 2);

    assert_eq!(
        runs[0]["workload"],
        serde_json::json!({ "apiVersion": "solti.io/v1", "kind": "Subprocess" })
    );
    assert_eq!(runs[0]["generation"], 1);
    assert_eq!(runs[0]["attempt"], 1);
    assert_eq!(runs[0]["phase"], "failed");
    assert_eq!(runs[0]["startedAt"], "2024-04-10T12:00:00Z");
    assert_eq!(runs[0]["finishedAt"], "2024-04-10T12:00:02Z");
    assert_eq!(runs[0]["error"], "exit code 1");
    assert_eq!(runs[0]["exitCode"], 1, "exitCode (int32) is a plain number");

    assert_eq!(runs[1]["generation"], 1);
    assert_eq!(runs[1]["attempt"], 2);
    assert_eq!(runs[1]["phase"], "succeeded");
    assert_eq!(runs[1]["startedAt"], "2024-04-10T12:00:05Z");
    assert_eq!(runs[1]["finishedAt"], "2024-04-10T12:00:06Z");
    assert_eq!(runs[1]["exitCode"], 0);
}

#[tokio::test]
async fn list_runs_rejects_embedded_history_leaked_by_custom_handler() {
    let app = router_with(Arc::new(WireMock {
        leak_embedded_run: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/embedded-task/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(resp).await["reason"], "InternalError");
}

#[tokio::test]
async fn sse_frames_match_documented_event_names_and_payloads() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/task-wire-1/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&bytes).unwrap();

    for frame in [
        "event: run-started\ndata: {\"type\":\"runStarted\",\"generation\":1,\"attempt\":1,\"startedAt\":1712750400000}",
        "event: chunk\ndata: {\"type\":\"chunk\",\"generation\":1,\"attempt\":1,\"stream\":\"stdout\",\"seq\":0,\"ts\":1712750400123,\"line\":\"hello world\"}",
        "event: run-finished\ndata: {\"type\":\"runFinished\",\"generation\":1,\"attempt\":1,\"exitCode\":0,\"finishedAt\":1712750400456}",
        "event: lagged\ndata: {\"type\":\"lagged\",\"skipped\":42}",
    ] {
        assert!(
            body.contains(frame),
            "missing expected SSE frame:\n{frame}\nin body:\n{body}"
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
        .oneshot(post_json("/apis/solti.io/v1/tasks", CREATE_COMMAND_BODY))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["apiVersion"], "v1");
    assert_eq!(body["kind"], "Status");
    assert_eq!(body["metadata"], serde_json::json!({}));
    assert_eq!(body["status"], "Failure");
    assert_eq!(body["reason"], "AlreadyExists");
    assert_eq!(body["code"], 409);
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
                .uri("/apis/solti.io/v1/tasks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "Unauthorized");
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
                .uri("/apis/solti.io/v1/tasks")
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}
