use super::*;

use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use solti_model::{
    ExtensionWorkload, OutputChunk, OutputEvent, StreamKind as ModelStreamKind, Task,
    TaskContinuation, TaskFilter, TaskId, TaskManifest, TaskPage, TaskPhase, TaskQuery, TaskRun,
    TaskRunPage, TaskRunQuery, TaskSpec, TaskWatchEvent, TaskWorkload, Uid, WORKLOAD_API_VERSION,
    WorkloadTypeMeta, WritePreconditions,
};

use crate::error::ApiError;
use crate::handler::{ApiHandler, OutputEventStream, TaskWatchEventStream};

#[derive(Default)]
struct StreamMock {
    last_preconditions: std::sync::Mutex<Option<WritePreconditions>>,
    last_query: std::sync::Mutex<Option<TaskQuery>>,
    query_calls: std::sync::atomic::AtomicUsize,
    query_resource_version: std::sync::Mutex<Option<String>>,
    last_run_query: std::sync::Mutex<Option<TaskRunQuery>>,
    query_items: std::sync::Mutex<Vec<Task>>,
    last_watch_filter: std::sync::Mutex<Option<TaskFilter>>,
    last_watch_resource_version: std::sync::Mutex<Option<Option<String>>>,
    watch_expired: bool,
    watch_stream_expired: bool,
    log_stream_pending: bool,
}

#[async_trait]
impl ApiHandler for StreamMock {
    async fn create_task(&self, _manifest: TaskManifest) -> Result<Task, ApiError> {
        unreachable!()
    }
    async fn apply_task(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        *self.last_preconditions.lock().unwrap() = Some(preconditions);
        Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
    }
    async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(None)
    }
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        self.query_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let requested_resource_version = query
            .continuation()
            .map(|continuation| continuation.resource_version().to_owned());
        *self.last_query.lock().unwrap() = Some(query);
        let resource_version = self
            .query_resource_version
            .lock()
            .unwrap()
            .clone()
            .or(requested_resource_version)
            .unwrap_or_else(|| "test:1".into());
        Ok(TaskPage {
            items: self.query_items.lock().unwrap().clone(),
            resource_version,
            continuation: None,
            remaining_item_count: 0,
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
        let mut events = vec![Ok(TaskWatchEvent::Added(watch_task()))];
        if self.watch_stream_expired {
            events.push(Err(ApiError::ResourceVersionExpired(
                "watch position is no longer retained".into(),
            )));
        }
        Ok(Box::pin(tokio_stream::iter(events)))
    }
    async fn query_task_runs(
        &self,
        id: &TaskId,
        query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError> {
        *self.last_run_query.lock().unwrap() = Some(query);
        let workload = if id.as_str() == "embedded-run" {
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Embedded").unwrap()
        } else {
            WorkloadTypeMeta::new("workloads.example.io/v1", "DatabaseBackup").unwrap()
        };
        Ok(TaskRunPage {
            items: vec![TaskRun::starting(2, 1, workload).unwrap()],
            task: id.clone(),
            task_uid: Uid::new("grpc-test-run-uid").unwrap(),
            resource_version: "runs-test:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }
    async fn cancel_task(
        &self,
        _id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        *self.last_preconditions.lock().unwrap() = Some(preconditions);
        Ok(())
    }
    async fn delete_task(
        &self,
        _id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        *self.last_preconditions.lock().unwrap() = Some(preconditions);
        Ok(())
    }
    async fn stream_task_logs(
        &self,
        id: &TaskId,
        _task_uid: &solti_model::Uid,
    ) -> Result<OutputEventStream, ApiError> {
        if id.as_str() == "missing" {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        if self.log_stream_pending {
            return Ok(Box::pin(tokio_stream::pending()));
        }
        let events = vec![
            OutputEvent::RunStarted {
                generation: 2,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1000),
            },
            OutputEvent::Chunk(OutputChunk {
                generation: 2,
                attempt: 1,
                stream: ModelStreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1100),
                line: Bytes::from_static(&[b'h', b'i', 0xff, 0xfe]),
                truncated: true,
            }),
            OutputEvent::RunFinished {
                generation: 2,
                attempt: 1,
                exit_code: Some(0),
                finished_at: UNIX_EPOCH + Duration::from_millis(1500),
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn service() -> TaskApiService<StreamMock> {
    TaskApiService::new(Arc::new(StreamMock::default()))
}

fn watch_task() -> Task {
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new(
            "workloads.example.io/v1",
            "ExampleJob",
            serde_json::json!({"value": 1}),
        )
        .unwrap(),
    );
    let spec = TaskSpec::builder("primary", workload, 5_000_u64)
        .build()
        .unwrap();
    let mut task = Task::new("watch-task", spec).unwrap();
    task.set_resource_version("test:2").unwrap();
    task
}

fn native_oversized_task() -> Task {
    let manifest = |padding: usize| {
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "workloads.example.io/v1",
                "LargePayload",
                serde_json::json!({ "padding": "x".repeat(padding) }),
            )
            .unwrap(),
        );
        let spec = TaskSpec::builder("large", workload, 5_000_u64)
            .build()
            .unwrap();
        TaskManifest::new("large", spec).unwrap()
    };
    let empty = manifest(0);
    let padding = solti_model::MAX_TASK_MANIFEST_BYTES
        .checked_sub(serde_json::to_vec(&empty).unwrap().len())
        .unwrap();
    let manifest = manifest(padding);
    assert_eq!(
        serde_json::to_vec(&manifest).unwrap().len(),
        solti_model::MAX_TASK_MANIFEST_BYTES
    );

    let mut task = Task::from_manifest(manifest).unwrap();
    task.set_resource_version("r".repeat(4 * 1024)).unwrap();
    task.validate().unwrap();
    task
}

#[tokio::test]
async fn get_task_maps_missing_resource_to_not_found_status() {
    let status = service()
        .get_task(Request::new(proto_api::GetTaskRequest {
            name: "missing".into(),
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn delete_task_forwards_write_preconditions() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    service
        .delete_task(Request::new(proto_api::DeleteTaskRequest {
            name: "task-1".into(),
            preconditions: Some(proto_api::WritePreconditions {
                uid: Some("uid-1".into()),
                resource_version: Some("17".into()),
            }),
        }))
        .await
        .unwrap();

    let preconditions = handler
        .last_preconditions
        .lock()
        .unwrap()
        .clone()
        .expect("handler received preconditions");
    assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
    assert_eq!(preconditions.resource_version(), Some("17"));
}

#[tokio::test]
async fn cancel_task_forwards_write_preconditions() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    service
        .cancel_task(Request::new(proto_api::CancelTaskRequest {
            name: "task-1".into(),
            preconditions: Some(proto_api::WritePreconditions {
                uid: Some("uid-1".into()),
                resource_version: Some("17".into()),
            }),
        }))
        .await
        .unwrap();

    let preconditions = handler
        .last_preconditions
        .lock()
        .unwrap()
        .clone()
        .expect("handler received preconditions");
    assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
    assert_eq!(preconditions.resource_version(), Some("17"));
}

#[tokio::test]
async fn cancel_task_rejects_empty_write_precondition() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    let status = service
        .cancel_task(Request::new(proto_api::CancelTaskRequest {
            name: "task-1".into(),
            preconditions: Some(proto_api::WritePreconditions {
                uid: None,
                resource_version: Some(String::new()),
            }),
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(handler.last_preconditions.lock().unwrap().is_none());
}

#[tokio::test]
async fn delete_task_rejects_empty_write_precondition() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    let status = service
        .delete_task(Request::new(proto_api::DeleteTaskRequest {
            name: "task-1".into(),
            preconditions: Some(proto_api::WritePreconditions {
                uid: None,
                resource_version: Some(String::new()),
            }),
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(handler.last_preconditions.lock().unwrap().is_none());
}

#[tokio::test]
async fn list_tasks_forwards_filters_and_continuation() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));
    let phases = vec![
        proto_api::TaskPhase::Pending as i32,
        proto_api::TaskPhase::Running as i32,
        proto_api::TaskPhase::Pending as i32,
    ];
    let label_selector = "environment=production,tier in (frontend,backend)";
    let filter = task_filter_from_wire(
        Some("primary".into()),
        phases.clone(),
        label_selector.into(),
    )
    .unwrap();
    let continuation =
        TaskContinuation::new("test:7", filter.clone(), TaskId::new("task-20").unwrap()).unwrap();

    service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            slot: Some("primary".into()),
            phases,
            limit: 25,
            label_selector: label_selector.into(),
            r#continue: crate::continuation::encode(continuation.clone()).unwrap(),
        }))
        .await
        .unwrap();

    let query = handler
        .last_query
        .lock()
        .unwrap()
        .take()
        .expect("handler received query");
    assert_eq!(query.slot().unwrap().as_str(), "primary");
    assert_eq!(query.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
    assert_eq!(query.limit(), 25);
    assert_eq!(
        query.item_byte_limit().get(),
        crate::MAX_TASK_LIST_RESPONSE_BYTES
    );
    assert_eq!(query.continuation(), Some(&continuation));
    assert_eq!(query.filter(), &filter);
    assert!(query.matches_labels(&{
        let mut labels = solti_model::Labels::new();
        labels
            .insert("environment", "production")
            .insert("tier", "backend");
        labels
    }));
}

#[tokio::test]
async fn list_tasks_rejects_a_cross_filter_token_before_the_handler() {
    use std::sync::atomic::Ordering;

    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));
    let continuation = TaskContinuation::new(
        "test:7",
        TaskFilter::new().with_phase(TaskPhase::Pending),
        TaskId::new("task-20").unwrap(),
    )
    .unwrap();

    let status = service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            phases: vec![proto_api::TaskPhase::Running as i32],
            r#continue: crate::continuation::encode(continuation).unwrap(),
            ..Default::default()
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert_eq!(handler.query_calls.load(Ordering::SeqCst), 0);
    assert!(handler.last_query.lock().unwrap().is_none());
}

#[tokio::test]
async fn list_tasks_rejects_a_custom_handler_page_from_another_snapshot() {
    use std::sync::atomic::Ordering;

    let handler = Arc::new(StreamMock {
        query_resource_version: std::sync::Mutex::new(Some("test:8".into())),
        ..StreamMock::default()
    });
    let service = TaskApiService::new(Arc::clone(&handler));
    let continuation =
        TaskContinuation::new("test:7", TaskFilter::new(), TaskId::new("task-20").unwrap())
            .unwrap();

    let status = service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            r#continue: crate::continuation::encode(continuation).unwrap(),
            ..Default::default()
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::Internal);
    assert_eq!(handler.query_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn list_tasks_maps_native_oversized_item_to_resource_exhausted() {
    let handler = Arc::new(StreamMock::default());
    handler
        .query_items
        .lock()
        .unwrap()
        .push(native_oversized_task());
    let service = TaskApiService::new(handler);

    let status = service
        .list_tasks(Request::new(proto_api::ListTasksRequest::default()))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::ResourceExhausted);
}

#[tokio::test]
async fn list_tasks_rejects_invalid_phase_or_label_selector_before_handler() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    let phase = service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            phases: vec![proto_api::TaskPhase::Unspecified as i32],
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(phase.code(), tonic::Code::InvalidArgument);

    let selector = service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            label_selector: "tier in (".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(selector.code(), tonic::Code::InvalidArgument);

    let continuation = service
        .list_tasks(Request::new(proto_api::ListTasksRequest {
            r#continue: "not-a-token".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(continuation.code(), tonic::Code::InvalidArgument);
    assert!(handler.last_query.lock().unwrap().is_none());
}

#[tokio::test]
async fn watch_tasks_forwards_filters_and_resource_version() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    let mut stream = service
        .watch_tasks(Request::new(proto_api::WatchTasksRequest {
            slot: Some("primary".into()),
            phases: vec![
                proto_api::TaskPhase::Pending as i32,
                proto_api::TaskPhase::Running as i32,
            ],
            label_selector: "environment=production".into(),
            resource_version: Some("test:1".into()),
        }))
        .await
        .unwrap()
        .into_inner();

    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(event.r#type, proto_api::TaskWatchEventType::Added as i32);
    assert_eq!(event.object.unwrap().metadata.unwrap().name, "watch-task");
    assert!(stream.next().await.is_none());

    let filter = handler
        .last_watch_filter
        .lock()
        .unwrap()
        .take()
        .expect("handler received watch filter");
    assert_eq!(filter.slot().unwrap().as_str(), "primary");
    assert_eq!(filter.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
    let mut labels = solti_model::Labels::new();
    labels.insert("environment", "production");
    assert!(filter.matches_labels(&labels));
    assert_eq!(
        handler.last_watch_resource_version.lock().unwrap().take(),
        Some(Some("test:1".into()))
    );
}

#[tokio::test]
async fn watch_tasks_maps_initial_expiration_to_out_of_range() {
    use std::sync::atomic::Ordering;

    let (probe, service) = probed_service_with(StreamMock {
        watch_expired: true,
        ..StreamMock::default()
    });

    let status = service
        .watch_tasks(Request::new(proto_api::WatchTasksRequest {
            resource_version: Some("old:1".into()),
            ..Default::default()
        }))
        .await
        .err()
        .expect("expired watch must fail");

    assert_eq!(status.code(), tonic::Code::OutOfRange);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        probe.last_status.load(Ordering::SeqCst),
        tonic::Code::OutOfRange as u16
    );
}

#[tokio::test]
async fn watch_tasks_maps_stream_expiration_to_out_of_range() {
    use std::sync::atomic::Ordering;

    let (probe, service) = probed_service_with(StreamMock {
        watch_stream_expired: true,
        ..StreamMock::default()
    });
    let mut stream = service
        .watch_tasks(Request::new(proto_api::WatchTasksRequest::default()))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);
    let status = stream.next().await.unwrap().unwrap_err();
    assert_eq!(status.code(), tonic::Code::OutOfRange);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        probe.last_status.load(Ordering::SeqCst),
        tonic::Code::OutOfRange as u16
    );
    assert!(stream.next().await.is_none());
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn watch_tasks_rejects_invalid_input_before_handler() {
    for request in [
        proto_api::WatchTasksRequest {
            resource_version: Some(String::new()),
            ..Default::default()
        },
        proto_api::WatchTasksRequest {
            phases: vec![proto_api::TaskPhase::Unspecified as i32],
            ..Default::default()
        },
        proto_api::WatchTasksRequest {
            label_selector: "tier in (".into(),
            ..Default::default()
        },
    ] {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));
        let status = service
            .watch_tasks(Request::new(request))
            .await
            .err()
            .expect("invalid watch must fail");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(handler.last_watch_filter.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn list_task_runs_exposes_historical_workload_gvk() {
    let response = service()
        .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
            name: "extension-run".into(),
            limit: 0,
            r#continue: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.runs.len(), 1);
    assert_eq!(
        response.runs[0].workload_api_version,
        "workloads.example.io/v1"
    );
    assert_eq!(response.runs[0].workload_kind, "DatabaseBackup");
    assert_eq!(response.task_uid, "grpc-test-run-uid");
    assert_eq!(response.resource_version, "runs-test:1");
    assert!(response.r#continue.is_empty());
    assert_eq!(response.remaining_item_count, None);
}

#[tokio::test]
async fn list_task_runs_forwards_default_count_and_native_byte_limits() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));

    service
        .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
            name: "extension-run".into(),
            limit: 0,
            r#continue: String::new(),
        }))
        .await
        .unwrap();

    let query = handler.last_run_query.lock().unwrap().clone().unwrap();
    assert_eq!(query.limit(), solti_model::DEFAULT_TASK_RUN_LIMIT);
    assert_eq!(
        query.item_byte_limit().get(),
        crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES
    );
}

#[tokio::test]
async fn list_task_runs_rejects_invalid_pagination_before_the_handler() {
    for request in [
        proto_api::ListTaskRunsRequest {
            name: "extension-run".into(),
            limit: solti_model::MAX_TASK_RUN_LIMIT as u32 + 1,
            r#continue: String::new(),
        },
        proto_api::ListTaskRunsRequest {
            name: "extension-run".into(),
            limit: 1,
            r#continue: "not-base64!".into(),
        },
    ] {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));
        let status = service
            .list_task_runs(Request::new(request))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(handler.last_run_query.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn list_task_runs_guards_embedded_history_from_custom_handler() {
    let status = service()
        .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
            name: "embedded-run".into(),
            limit: 0,
            r#continue: String::new(),
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::Internal);
}

#[tokio::test]
async fn list_task_runs_rejects_a_cross_task_token_before_the_handler() {
    let handler = Arc::new(StreamMock::default());
    let service = TaskApiService::new(Arc::clone(&handler));
    let continuation = solti_model::TaskRunContinuation::new(
        "runs-test:1",
        TaskId::new("other-task").unwrap(),
        Uid::new("other-task-uid").unwrap(),
        1,
        1,
    )
    .unwrap();

    let status = service
        .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
            name: "requested-task".into(),
            limit: 1,
            r#continue: crate::continuation::encode_run(continuation).unwrap(),
        }))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(handler.last_run_query.lock().unwrap().is_none());
}

#[tokio::test]
async fn stream_task_logs_returns_three_proto_events_in_order() {
    let svc = service();
    let req = Request::new(proto_api::StreamTaskLogsRequest {
        name: "task-1".into(),
        task_uid: "task-1-uid".into(),
    });

    let response = svc.stream_task_logs(req).await.expect("stream Ok");
    let mut stream = response.into_inner();

    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(event.task_uid, "task-1-uid");
    match event.kind.unwrap() {
        proto_api::stream_task_logs_response::Kind::RunStarted(r) => {
            assert_eq!(r.generation, 2);
            assert_eq!(r.attempt, 1);
            assert_eq!(r.started_at, 1000);
        }
        other => panic!("expected RunStarted, got {other:?}"),
    }

    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(event.task_uid, "task-1-uid");
    match event.kind.unwrap() {
        proto_api::stream_task_logs_response::Kind::Chunk(c) => {
            assert_eq!(c.generation, 2);
            assert_eq!(c.attempt, 1);
            assert_eq!(c.stream, proto_api::OutputStreamKind::Stdout as i32);
            assert_eq!(c.seq, 0);
            assert_eq!(&c.line[..], &[b'h', b'i', 0xff, 0xfe]);
            assert!(c.truncated);
        }
        other => panic!("expected Chunk, got {other:?}"),
    }

    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(event.task_uid, "task-1-uid");
    match event.kind.unwrap() {
        proto_api::stream_task_logs_response::Kind::RunFinished(r) => {
            assert_eq!(r.generation, 2);
            assert_eq!(r.attempt, 1);
            assert_eq!(r.exit_code, Some(0));
            assert_eq!(r.finished_at, 1500);
        }
        other => panic!("expected RunFinished, got {other:?}"),
    }
    assert!(stream.next().await.is_none(), "stream must terminate");
}

#[tokio::test]
async fn stream_task_logs_rejects_every_invalid_model_name() {
    let svc = service();
    for invalid in ["  ", "a/b", "a b", ".", "bad$name"] {
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            name: invalid.into(),
            task_uid: "task-1-uid".into(),
        });
        let status = match svc.stream_task_logs(req).await {
            Err(s) => s,
            Ok(_) => panic!("expected error status for {invalid:?}"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}

#[tokio::test]
async fn stream_task_logs_maps_task_not_found_to_not_found_status() {
    let svc = service();
    let req = Request::new(proto_api::StreamTaskLogsRequest {
        name: "missing".into(),
        task_uid: "missing-uid".into(),
    });
    let status = match svc.stream_task_logs(req).await {
        Err(s) => s,
        Ok(_) => panic!("expected error status"),
    };
    assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn stream_task_logs_rejects_an_empty_task_uid() {
    let status = match service()
        .stream_task_logs(Request::new(proto_api::StreamTaskLogsRequest {
            name: "task-1".into(),
            task_uid: String::new(),
        }))
        .await
    {
        Err(status) => status,
        Ok(_) => panic!("expected invalid task UID"),
    };
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

fn authenticated_service(secret: &str) -> TaskApiService<StreamMock> {
    let authenticator: ApiAuthenticatorHandle =
        Arc::new(StaticBearerAuthenticator::new(Token::new(secret).unwrap()));
    TaskApiService::new_with_access(
        Arc::new(StreamMock::default()),
        noop_api_metrics(),
        Some(authenticator),
        None,
    )
}

fn list_request_with_authorization(value: Option<&str>) -> Request<proto_api::ListTasksRequest> {
    let mut req = Request::new(proto_api::ListTasksRequest::default());
    if let Some(value) = value {
        req.metadata_mut()
            .insert("authorization", value.parse().expect("ascii metadata"));
    }
    req
}

#[tokio::test]
async fn bearer_auth_rejects_invalid_credentials() {
    let headers = [
        None,
        Some("Bearer not-the-secret"),
        Some("sekret"),
        Some("Basic sekret"),
    ];

    for header in headers {
        let status = authenticated_service("sekret")
            .list_tasks(list_request_with_authorization(header))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }
}

#[tokio::test]
async fn bearer_auth_accepts_valid_token_scheme_case_insensitively() {
    for header in ["Bearer sekret", "bearer sekret", "BEARER sekret"] {
        assert!(
            authenticated_service("sekret")
                .list_tasks(list_request_with_authorization(Some(header)))
                .await
                .is_ok(),
            "header {header:?} must pass"
        );
    }
}

#[tokio::test]
async fn bearer_auth_passes_through_when_no_authenticator_is_configured() {
    let svc = service();
    assert!(
        svc.list_tasks(list_request_with_authorization(None))
            .await
            .is_ok()
    );
    assert!(
        svc.list_tasks(list_request_with_authorization(Some("Bearer anything")))
            .await
            .is_ok()
    );
}

#[test]
fn static_bearer_interceptor_installs_an_authenticated_identity() {
    let mut auth = BearerAuth {
        expected: Some(Token::new("sekret").unwrap()),
        metrics: noop_api_metrics(),
    };
    let mut request = Request::new(());
    request
        .metadata_mut()
        .insert("authorization", "Bearer sekret".parse().unwrap());

    let request = auth.call(request).unwrap();
    let identity = request.extensions().get::<ApiIdentity>().unwrap();
    assert_eq!(identity.subject(), None);
}

struct SubjectAuthenticator;

#[async_trait]
impl crate::ApiAuthenticator for SubjectAuthenticator {
    async fn authenticate(
        &self,
        request: AuthenticationRequest<'_>,
    ) -> Result<ApiIdentity, ApiError> {
        if request.transport() == Transport::Grpc
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
impl crate::ApiAuthorizer for RecordingAuthorizer {
    async fn authorize(&self, request: AuthorizationRequest<'_>) -> Result<(), ApiError> {
        let target = match request.target() {
            TaskTarget::Collection => "collection".to_owned(),
            TaskTarget::Task(task) => task.to_string(),
            TaskTarget::Manifest(manifest) => manifest.name().to_string(),
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
async fn custom_access_hooks_propagate_identity_and_deny_before_handler() {
    let authenticator: ApiAuthenticatorHandle = Arc::new(SubjectAuthenticator);
    let recording = Arc::new(RecordingAuthorizer::default());
    let authorizer: ApiAuthorizerHandle = recording.clone();
    let service = TaskApiService::new_with_access(
        Arc::new(StreamMock::default()),
        noop_api_metrics(),
        Some(authenticator),
        Some(authorizer),
    );

    service
        .list_tasks(list_request_with_authorization(Some(
            "Bearer subject-token",
        )))
        .await
        .unwrap();

    let mut cancel = Request::new(proto_api::CancelTaskRequest {
        name: "task-a".into(),
        preconditions: None,
    });
    cancel
        .metadata_mut()
        .insert("authorization", "Bearer subject-token".parse().unwrap());
    service.cancel_task(cancel).await.unwrap();

    let mut logs = Request::new(proto_api::StreamTaskLogsRequest {
        name: "task-a".into(),
        task_uid: "task-a-uid".into(),
    });
    logs.metadata_mut()
        .insert("authorization", "Bearer subject-token".parse().unwrap());
    let status = match service.stream_task_logs(logs).await {
        Err(status) => status,
        Ok(_) => panic!("authorization must deny the log stream"),
    };
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

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

#[derive(Debug, Default)]
struct GaugeProbe {
    in_flight: std::sync::atomic::AtomicI64,
    completed: std::sync::atomic::AtomicUsize,
    last_status: std::sync::atomic::AtomicU16,
}

impl crate::metrics::ApiMetricsBackend for GaugeProbe {
    fn record_request(
        &self,
        _transport: crate::metrics::Transport,
        _method: &str,
        _path: &str,
        status: u16,
        _duration_ms: u64,
    ) {
        self.last_status
            .store(status, std::sync::atomic::Ordering::SeqCst);
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn record_in_flight_delta(&self, _transport: crate::metrics::Transport, delta: i64) {
        self.in_flight
            .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
    }
}

fn probed_service() -> (Arc<GaugeProbe>, TaskApiService<StreamMock>) {
    probed_service_with(StreamMock::default())
}

fn probed_service_with(handler: StreamMock) -> (Arc<GaugeProbe>, TaskApiService<StreamMock>) {
    let probe = Arc::new(GaugeProbe::default());
    let handle: ApiMetricsHandle = probe.clone();
    (
        probe,
        TaskApiService::new_with_metrics(Arc::new(handler), handle),
    )
}

#[tokio::test]
async fn rejected_auth_is_recorded_and_balances_gauge() {
    use std::sync::atomic::Ordering;

    let probe = Arc::new(GaugeProbe::default());
    let metrics: ApiMetricsHandle = probe.clone();
    let authenticator: ApiAuthenticatorHandle = Arc::new(StaticBearerAuthenticator::new(
        Token::new("secret").unwrap(),
    ));
    let service = TaskApiService::new_with_access(
        Arc::new(StreamMock::default()),
        metrics,
        Some(authenticator),
        None,
    );

    let status = service
        .list_tasks(list_request_with_authorization(None))
        .await
        .unwrap_err();

    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn instrument_records_completed_request_and_balances_gauge() {
    use std::sync::atomic::Ordering;

    let (probe, svc) = probed_service();
    let result = svc
        .instrument("Probe", async { Ok(Response::new(())) })
        .await;

    assert!(result.is_ok());
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_subscription_is_instrumented() {
    use std::sync::atomic::Ordering;

    let (probe, service) = probed_service();
    let mut stream = service
        .stream_task_logs(Request::new(proto_api::StreamTaskLogsRequest {
            name: "task-a".into(),
            task_uid: "task-a-uid".into(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);

    while let Some(event) = stream.next().await {
        event.unwrap();
    }

    assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    assert_eq!(
        probe.last_status.load(Ordering::SeqCst),
        tonic::Code::Ok as u16
    );
}

#[tokio::test]
async fn dropping_server_stream_releases_gauge_without_completion() {
    use std::sync::atomic::Ordering;

    let (probe, service) = probed_service_with(StreamMock {
        log_stream_pending: true,
        ..StreamMock::default()
    });
    let response = service
        .stream_task_logs(Request::new(proto_api::StreamTaskLogsRequest {
            name: "task-a".into(),
            task_uid: "task-a-uid".into(),
        }))
        .await
        .unwrap();

    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);

    drop(response);

    assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
    assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
}

#[test]
fn in_flight_gauge_recovers_when_rpc_future_is_dropped() {
    use std::future::Future;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll, Waker};

    let (probe, svc) = probed_service();

    let mut fut = Box::pin(svc.instrument(
        "Probe",
        std::future::pending::<Result<Response<()>, Status>>(),
    ));

    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(
        probe.in_flight.load(Ordering::SeqCst),
        1,
        "gauge must be armed after the first poll"
    );

    drop(fut);
    assert_eq!(
        probe.in_flight.load(Ordering::SeqCst),
        0,
        "dropping the future must release the in-flight slot"
    );
    assert_eq!(
        probe.completed.load(Ordering::SeqCst),
        0,
        "a cancelled RPC must not be recorded as completed"
    );
}
