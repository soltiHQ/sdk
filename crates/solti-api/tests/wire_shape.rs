//! Wire-shape pinning tests for the HTTP transport.
//!
//! The tests replay CRD-shaped resources through the real router and pin the
//! model-owned JSON contract key by key.

#![cfg(feature = "http")]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use solti_api::{ApiError, ApiHandler, HttpApi, OutputEventStream, TaskWatchEventStream};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, Labels, OutputChunk,
    OutputEvent, RestartPolicy, StreamKind, SubprocessMode, SubprocessSpec, Task, TaskContinuation,
    TaskEnv, TaskFilter, TaskId, TaskManifest, TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec,
    TaskWatchEvent, TaskWorkload, Token, WorkloadTypeMeta, WritePreconditions,
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
            "interpreter": "bash",
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

/// A script with an explicitly selected interpreter.
const CREATE_CUSTOM_INTERPRETER_BODY: &str = r#"{
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
            "interpreter": "ruby",
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

/// Task fixture used to pin the public resource shape.
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

    let mut labels = Labels::new();
    labels
        .insert("environment", "production")
        .insert("tier", "backend");
    let manifest = TaskManifest::new("task-wire-1", spec)
        .unwrap()
        .with_labels(labels)
        .unwrap();
    let mut task = Task::from_manifest(manifest).expect("fixture task must be valid");
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

/// Run fixtures used to pin the public history shape.
fn fixture_runs() -> Vec<TaskRun> {
    let workload = WorkloadTypeMeta::new("solti.io/v1", "Subprocess").unwrap();
    let failed = TaskRun::from_parts(
        workload.clone(),
        1,
        1,
        TaskPhase::Failed,
        UNIX_EPOCH + Duration::from_millis(1_712_750_400_000),
        Some(UNIX_EPOCH + Duration::from_millis(1_712_750_402_000)),
        Some("exit code 1".into()),
        Some(1),
    )
    .unwrap();

    let succeeded = TaskRun::from_parts(
        workload,
        1,
        2,
        TaskPhase::Succeeded,
        UNIX_EPOCH + Duration::from_millis(1_712_750_405_000),
        Some(UNIX_EPOCH + Duration::from_millis(1_712_750_406_000)),
        None,
        Some(0),
    )
    .unwrap();

    vec![failed, succeeded]
}

#[derive(Default)]
struct WireMock {
    last_admission: Mutex<Option<AdmissionPolicy>>,
    last_query: Mutex<Option<TaskQuery>>,
    last_watch_filter: Mutex<Option<TaskFilter>>,
    last_watch_resource_version: Mutex<Option<Option<String>>>,
    last_write_preconditions: Mutex<Option<WritePreconditions>>,
    submit_conflicts: bool,
    leak_embedded_task: bool,
    leak_embedded_run: bool,
    non_utf8_output: bool,
    watch_expired: bool,
    watch_error_then_pending: bool,
    watch_stream_expired: bool,
}

struct ErrorThenPendingWatch {
    error: Option<ApiError>,
}

impl tokio_stream::Stream for ErrorThenPendingWatch {
    type Item = Result<TaskWatchEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.error.take() {
            Some(error) => Poll::Ready(Some(Err(error))),
            None => Poll::Pending,
        }
    }
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

    async fn apply_task(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        *self.last_admission.lock().unwrap() = Some(manifest.spec().admission());
        *self.last_write_preconditions.lock().unwrap() = Some(preconditions);
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

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        let item = if self.leak_embedded_task {
            embedded_task()
        } else {
            fixture_task()
        };
        let matches = query.matches(&item);
        let continuation = matches
            .then(|| TaskContinuation::new("test:5", query.filter().clone(), item.name().clone()))
            .transpose()
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        *self.last_query.lock().unwrap() = Some(query);
        Ok(TaskPage {
            items: matches.then_some(item).into_iter().collect(),
            resource_version: "test:5".into(),
            continuation,
            remaining_item_count: if matches { 2 } else { 0 },
        })
    }

    async fn watch_tasks(
        &self,
        filter: TaskFilter,
        resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        *self.last_watch_filter.lock().unwrap() = Some(filter);
        *self.last_watch_resource_version.lock().unwrap() = Some(resource_version);
        if self.watch_expired {
            return Err(ApiError::ResourceVersionExpired(
                "requested resourceVersion is no longer retained".into(),
            ));
        }
        if self.watch_error_then_pending {
            return Ok(Box::pin(ErrorThenPendingWatch {
                error: Some(ApiError::ResourceVersionExpired(
                    "watch position is no longer retained".into(),
                )),
            }));
        }

        let task = if self.leak_embedded_task {
            embedded_task()
        } else {
            fixture_task()
        };
        let mut events = vec![
            Ok(TaskWatchEvent::Added(task.clone())),
            Ok(TaskWatchEvent::Modified(task.clone())),
            Ok(TaskWatchEvent::Deleted(task)),
        ];
        if self.watch_stream_expired {
            events.push(Err(ApiError::ResourceVersionExpired(
                "watch position is no longer retained".into(),
            )));
        }
        Ok(Box::pin(tokio_stream::iter(events)))
    }

    async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        if self.leak_embedded_run {
            return Ok(vec![
                TaskRun::starting(
                    1,
                    1,
                    WorkloadTypeMeta::new("solti.io/v1", "Embedded").unwrap(),
                )
                .unwrap(),
            ]);
        }
        Ok(fixture_runs())
    }

    async fn delete_task(
        &self,
        _id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        *self.last_write_preconditions.lock().unwrap() = Some(preconditions);
        Ok(())
    }

    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        // Fixtures pinning the SSE event contract.
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
                line: if self.non_utf8_output {
                    Bytes::from_static(&[b'h', b'i', 0xFF, 0xFE])
                } else {
                    Bytes::from_static(b"hello world")
                },
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
async fn create_custom_interpreter_resource_is_accepted() {
    let app = router_with(Arc::new(WireMock::default()));

    let resp = app
        .oneshot(post_json(
            "/apis/solti.io/v1/tasks",
            CREATE_CUSTOM_INTERPRETER_BODY,
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
async fn create_rejects_unknown_crd_fields_before_the_handler() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));
    let mut body: Value = serde_json::from_str(CREATE_COMMAND_BODY).unwrap();
    body["unexpected"] = serde_json::json!(true);

    let resp = app
        .oneshot(post_json(
            "/apis/solti.io/v1/tasks",
            &serde_json::to_string(&body).unwrap(),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["reason"], "BadRequest");
    assert_eq!(*handler.last_admission.lock().unwrap(), None);
}

#[tokio::test]
async fn create_rejects_unknown_script_field() {
    let app = router_with(Arc::new(WireMock::default()));
    let body = CREATE_CUSTOM_INTERPRETER_BODY.replace(
        r#""interpreter": "ruby""#,
        r#""interpreter": "ruby", "flag": "-e""#,
    );
    assert_ne!(body, CREATE_CUSTOM_INTERPRETER_BODY, "replacement must hit");

    let resp = app
        .oneshot(post_json("/apis/solti.io/v1/tasks", &body))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["reason"], "BadRequest");
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
async fn apply_forwards_write_preconditions() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/apis/solti.io/v1/tasks/task-wire-1?uid=uid-1&resourceVersion=17")
                .header("content-type", "application/json")
                .body(Body::from(APPLY_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let preconditions = handler
        .last_write_preconditions
        .lock()
        .unwrap()
        .clone()
        .expect("handler received preconditions");
    assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
    assert_eq!(preconditions.resource_version(), Some("17"));
}

#[tokio::test]
async fn delete_forwards_write_preconditions() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/apis/solti.io/v1/tasks/task-wire-1?uid=uid-1&resourceVersion=17")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let preconditions = handler
        .last_write_preconditions
        .lock()
        .unwrap()
        .clone()
        .expect("handler received preconditions");
    assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
    assert_eq!(preconditions.resource_version(), Some("17"));
}

#[tokio::test]
async fn write_rejects_empty_precondition_before_the_handler() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/apis/solti.io/v1/tasks/task-wire-1?resourceVersion=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(handler.last_write_preconditions.lock().unwrap().is_none());
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
    assert_eq!(reconciled["message"], "runtime accepted the desired state");
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
    assert_eq!(body["metadata"]["resourceVersion"], "test:5");
    assert_eq!(body["metadata"]["remainingItemCount"], 2);
    assert!(
        body["metadata"]["continue"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
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
async fn list_tasks_forwards_repeated_phases_and_kubernetes_label_selector() {
    let handler = Arc::new(WireMock::default());
    let query_params = "slot=my-job&phase=failed&phase=succeeded&phase=failed&labelSelector=environment%3Dproduction%2Ctier%20in%20%28frontend%2Cbackend%29&limit=25";

    let first = router_with(Arc::clone(&handler))
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/apis/solti.io/v1/tasks?{query_params}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first.status(), StatusCode::OK);
    let first_body = body_json(first).await;
    let token = first_body["metadata"]["continue"]
        .as_str()
        .expect("first page carries continue")
        .to_owned();
    let first_query = handler
        .last_query
        .lock()
        .unwrap()
        .take()
        .expect("handler received query");
    assert_eq!(first_query.slot().unwrap().as_str(), "my-job");
    assert_eq!(
        first_query.phases(),
        &[TaskPhase::Failed, TaskPhase::Succeeded]
    );
    assert_eq!(first_query.limit(), 25);
    assert!(first_query.continuation().is_none());
    let mut labels = Labels::new();
    labels
        .insert("environment", "production")
        .insert("tier", "backend");
    assert!(first_query.matches_labels(&labels));

    let next = router_with(Arc::clone(&handler))
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/apis/solti.io/v1/tasks?{query_params}&continue={token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(next.status(), StatusCode::OK);
    let next_query = handler
        .last_query
        .lock()
        .unwrap()
        .take()
        .expect("handler received continuation query");
    let continuation = next_query
        .continuation()
        .expect("continue token decoded into domain cursor");
    assert_eq!(continuation.resource_version(), "test:5");
    assert_eq!(continuation.after().as_str(), "task-wire-1");
    assert_eq!(continuation.filter(), next_query.filter());
}

#[tokio::test]
async fn list_tasks_rejects_invalid_or_ambiguous_query_before_handler() {
    for query in [
        "phase=",
        "labelSelector=tier%20in%20%28",
        "continue=",
        "continue=not-a-token",
        "slot=one&slot=two",
        "unknown=value",
    ] {
        let handler = Arc::new(WireMock::default());
        let app = router_with(Arc::clone(&handler));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/apis/solti.io/v1/tasks?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "query: {query}");
        assert!(handler.last_query.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn watch_tasks_uses_kubernetes_documents_and_forwards_filters() {
    let handler = Arc::new(WireMock::default());
    let app = router_with(Arc::clone(&handler));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=1&resourceVersion=test%3A4&slot=primary&phase=pending&phase=running&labelSelector=environment%3Dproduction")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let documents = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 3);
    assert_eq!(documents[0]["type"], "ADDED");
    assert_eq!(documents[1]["type"], "MODIFIED");
    assert_eq!(documents[2]["type"], "DELETED");
    assert_eq!(documents[0]["object"]["metadata"]["name"], "task-wire-1");

    let filter = handler
        .last_watch_filter
        .lock()
        .unwrap()
        .take()
        .expect("handler received watch filter");
    assert_eq!(filter.slot().unwrap().as_str(), "primary");
    assert_eq!(filter.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
    let mut labels = Labels::new();
    labels.insert("environment", "production");
    assert!(filter.matches_labels(&labels));
    assert_eq!(
        handler.last_watch_resource_version.lock().unwrap().take(),
        Some(Some("test:4".into()))
    );
}

#[tokio::test]
async fn watch_tasks_maps_initial_expiration_to_http_gone() {
    let app = router_with(Arc::new(WireMock {
        watch_expired: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=true&resourceVersion=old%3A1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::GONE);
    let body = body_json(resp).await;
    assert_eq!(body["reason"], "Expired");
    assert_eq!(body["code"], 410);
}

#[tokio::test]
async fn watch_tasks_encodes_stream_error_as_final_error_document() {
    let app = router_with(Arc::new(WireMock {
        watch_stream_expired: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let documents = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 4);
    assert_eq!(documents[3]["type"], "ERROR");
    assert_eq!(documents[3]["object"]["reason"], "Expired");
    assert_eq!(documents[3]["object"]["code"], 410);
}

#[tokio::test]
async fn watch_tasks_closes_after_error_without_polling_pending_source() {
    let app = router_with(Arc::new(WireMock {
        watch_error_then_pending: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = tokio::time::timeout(Duration::from_secs(1), resp.into_body().collect())
        .await
        .expect("HTTP watch must reach EOF after its final ERROR")
        .unwrap()
        .to_bytes();
    let documents = body
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["type"], "ERROR");
    assert_eq!(documents[0]["object"]["reason"], "Expired");
}

#[tokio::test]
async fn watch_tasks_rejects_pagination_and_ambiguous_parameters() {
    for query in [
        "watch=true&limit=0",
        "watch=true&continue=opaque",
        "watch=true&offset=0",
        "watch=true&watch=1",
        "watch=invalid",
        "watch=true&resourceVersion=",
        "resourceVersion=test%3A1",
    ] {
        let handler = Arc::new(WireMock::default());
        let app = router_with(Arc::clone(&handler));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/apis/solti.io/v1/tasks?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "query: {query}");
        assert!(handler.last_watch_filter.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn watch_tasks_stops_after_handler_leaks_embedded_task() {
    let app = router_with(Arc::new(WireMock {
        leak_embedded_task: true,
        ..WireMock::default()
    }));

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks?watch=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let documents = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["type"], "ERROR");
    assert_eq!(documents[0]["object"]["reason"], "InternalError");
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
        "event: chunk\ndata: {\"type\":\"chunk\",\"generation\":1,\"attempt\":1,\"stream\":\"stdout\",\"seq\":0,\"ts\":1712750400123,\"line\":\"aGVsbG8gd29ybGQ=\"}",
        "event: run-finished\ndata: {\"type\":\"runFinished\",\"generation\":1,\"attempt\":1,\"exitCode\":0,\"finishedAt\":1712750400456}",
        "event: lagged\ndata: {\"type\":\"lagged\",\"skipped\":42}",
    ] {
        assert!(
            body.contains(frame),
            "missing expected SSE frame:\n{frame}\nin body:\n{body}"
        );
    }
}

#[tokio::test]
async fn sse_preserves_non_utf8_output_as_base64() {
    let app = router_with(Arc::new(WireMock {
        non_utf8_output: true,
        ..WireMock::default()
    }));

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/apis/solti.io/v1/tasks/task-wire-1/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();

    assert!(body.contains(r#""line":"aGn//g==""#), "{body}");
    assert!(!body.contains('\u{FFFD}'), "{body}");
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
        .with_auth(Token::new("secret-token").unwrap())
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
        .with_auth(Token::new("secret-token").unwrap())
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
