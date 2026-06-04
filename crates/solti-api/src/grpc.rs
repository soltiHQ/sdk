//! # gRPC transport.
//!
//! [`TaskApiService`] implements the generated `TaskService` trait from `proto/solti/task/v1/api.proto`, delegating to an [`ApiHandler`](crate::ApiHandler).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use tracing::debug;

use solti_model::TaskQuery;

use crate::convert::{output_event_to_proto, proto_to_domain_status, tasks_page_to_proto};
use crate::error::ApiError;
use crate::handler::ApiHandler;
use crate::metrics::{ApiMetricsHandle, Transport, noop_api_metrics};
use crate::proto_api::{
    self, task_service_server::TaskService, task_service_server::TaskServiceServer,
};
use crate::validate::{clamp_list_limit, non_empty_id};

/// gRPC service wrapping an [`ApiHandler`](crate::ApiHandler).
///
/// ## Also
///
/// - `TaskServiceServer` generated tonic server wrapper.
/// - [`ApiError`](crate::ApiError) mapped to `tonic::Status`.
pub struct TaskApiService<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
}

impl<H> TaskApiService<H>
where
    H: ApiHandler,
{
    /// Create a new gRPC service with the given handler and no-op metrics.
    pub fn new(handler: Arc<H>) -> Self {
        Self::new_with_metrics(handler, noop_api_metrics())
    }

    /// Create a new gRPC service with an explicit metrics backend.
    pub fn new_with_metrics(handler: Arc<H>, metrics: ApiMetricsHandle) -> Self {
        Self { handler, metrics }
    }

    async fn instrument<F, T>(&self, method: &'static str, fut: F) -> Result<Response<T>, Status>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        self.metrics.record_in_flight_delta(Transport::Grpc, 1);
        let start = Instant::now();
        let result = fut.await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = match &result {
            Ok(_) => 0u16,
            Err(s) => s.code() as u16,
        };
        let path = format!("/solti.task.v1.TaskService/{}", method);
        self.metrics
            .record_request(Transport::Grpc, method, &path, status, duration_ms);
        self.metrics.record_in_flight_delta(Transport::Grpc, -1);
        result
    }
}

/// Build a configured `TaskServiceServer` with no-op metrics.
///
/// ## Example
///
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use solti_api::{build_grpc_server, SupervisorApiAdapter};
/// # async fn example(adapter: Arc<SupervisorApiAdapter>) -> Result<(), Box<dyn std::error::Error>> {
/// let svc = build_grpc_server(adapter);
/// tonic::transport::Server::builder()
///     .add_service(svc)
///     .serve("0.0.0.0:50052".parse()?)
///     .await?;
/// # Ok(()) }
/// ```
pub fn build_grpc_server<H>(handler: Arc<H>) -> TaskServiceServer<TaskApiService<H>>
where
    H: ApiHandler,
{
    build_grpc_server_with_metrics(handler, noop_api_metrics())
}

/// Build a configured `TaskServiceServer` with an explicit metrics backend.
pub fn build_grpc_server_with_metrics<H>(
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
) -> TaskServiceServer<TaskApiService<H>>
where
    H: ApiHandler,
{
    TaskServiceServer::new(TaskApiService::new_with_metrics(handler, metrics))
        .max_decoding_message_size(crate::MAX_REQUEST_BYTES)
        .max_encoding_message_size(crate::MAX_REQUEST_BYTES)
}

#[tonic::async_trait]
impl<H> TaskService for TaskApiService<H>
where
    H: ApiHandler,
{
    async fn submit_task(
        &self,
        request: Request<proto_api::SubmitTaskRequest>,
    ) -> Result<Response<proto_api::SubmitTaskResponse>, Status> {
        self.instrument("SubmitTask", async move {
            let req = request.into_inner();

            let spec = req
                .spec
                .ok_or_else(|| Status::invalid_argument("missing spec"))?;

            let spec =
                crate::convert::convert_create_spec(spec).map_err(|e: ApiError| Status::from(e))?;

            debug!(slot = %spec.slot(), kind = ?spec.kind(), "grpc: submitting task");
            let task_id = self.handler.submit_task(spec).await.map_err(Status::from)?;

            Ok(Response::new(proto_api::SubmitTaskResponse {
                task_id: task_id.to_string(),
            }))
        })
        .await
    }

    async fn apply_task(
        &self,
        request: Request<proto_api::ApplyTaskRequest>,
    ) -> Result<Response<proto_api::ApplyTaskResponse>, Status> {
        self.instrument("ApplyTask", async move {
            let req = request.into_inner();

            let spec = req
                .spec
                .ok_or_else(|| Status::invalid_argument("missing spec"))?;

            let spec =
                crate::convert::convert_create_spec(spec).map_err(|e: ApiError| Status::from(e))?;

            debug!(slot = %spec.slot(), kind = ?spec.kind(), "grpc: applying task");
            let task_id = self.handler.apply_task(spec).await.map_err(Status::from)?;

            Ok(Response::new(proto_api::ApplyTaskResponse {
                task_id: task_id.to_string(),
            }))
        })
        .await
    }

    async fn get_task_status(
        &self,
        request: Request<proto_api::GetTaskStatusRequest>,
    ) -> Result<Response<proto_api::GetTaskStatusResponse>, Status> {
        self.instrument("GetTaskStatus", async move {
            let req = request.into_inner();

            non_empty_id("task_id", &req.task_id).map_err(Status::from)?;

            let task_id = solti_model::TaskId::from(req.task_id);
            debug!(%task_id, "grpc: getting task status");

            let info = self
                .handler
                .get_task_status(&task_id)
                .await
                .map_err(Status::from)?;

            let task = info
                .map(proto_api::TaskData::try_from)
                .transpose()
                .map_err(Status::from)?;

            Ok(Response::new(proto_api::GetTaskStatusResponse { task }))
        })
        .await
    }

    async fn list_tasks(
        &self,
        request: Request<proto_api::ListTasksRequest>,
    ) -> Result<Response<proto_api::ListTasksResponse>, Status> {
        self.instrument("ListTasks", async move {
            let req = request.into_inner();

            let mut query = TaskQuery::new();

            if let Some(slot) = req.slot {
                non_empty_id("slot", &slot).map_err(Status::from)?;
                query = query.with_slot(slot);
            }

            if let Some(status_raw) = req.status {
                let status = proto_to_domain_status(status_raw).map_err(Status::from)?;
                query = query.with_status(status);
            }

            query = query.with_limit(clamp_list_limit(req.limit));
            if req.offset > 0 {
                query = query.with_offset(req.offset as usize);
            }

            let page = self
                .handler
                .query_tasks(query)
                .await
                .map_err(Status::from)?;

            debug!(
                count = page.items.len(),
                total = page.total,
                "grpc: tasks listed"
            );

            let response = tasks_page_to_proto(page).map_err(Status::from)?;
            Ok(Response::new(response))
        })
        .await
    }

    async fn list_task_runs(
        &self,
        request: Request<proto_api::ListTaskRunsRequest>,
    ) -> Result<Response<proto_api::ListTaskRunsResponse>, Status> {
        self.instrument("ListTaskRuns", async move {
            let req = request.into_inner();

            non_empty_id("task_id", &req.task_id).map_err(Status::from)?;

            let task_id = solti_model::TaskId::from(req.task_id);
            debug!(%task_id, "grpc: listing task runs");

            let runs = self
                .handler
                .list_task_runs(&task_id)
                .await
                .map_err(Status::from)?;

            let runs = runs.into_iter().map(proto_api::TaskRunInfo::from).collect();

            Ok(Response::new(proto_api::ListTaskRunsResponse { runs }))
        })
        .await
    }

    async fn delete_task(
        &self,
        request: Request<proto_api::DeleteTaskRequest>,
    ) -> Result<Response<proto_api::DeleteTaskResponse>, Status> {
        self.instrument("DeleteTask", async move {
            let req = request.into_inner();

            non_empty_id("task_id", &req.task_id).map_err(Status::from)?;

            let task_id = solti_model::TaskId::from(req.task_id);
            debug!(%task_id, "grpc: deleting task");

            self.handler
                .delete_task(&task_id)
                .await
                .map_err(Status::from)?;

            debug!(%task_id, "grpc: task deleted");
            Ok(Response::new(proto_api::DeleteTaskResponse {}))
        })
        .await
    }

    /// Server-streaming RPC.
    type StreamTaskLogsStream = Pin<
        Box<
            dyn tokio_stream::Stream<Item = Result<proto_api::StreamTaskLogsResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn stream_task_logs(
        &self,
        request: Request<proto_api::StreamTaskLogsRequest>,
    ) -> Result<Response<Self::StreamTaskLogsStream>, Status> {
        let req = request.into_inner();
        non_empty_id("task_id", &req.task_id).map_err(Status::from)?;

        let task_id = solti_model::TaskId::from(req.task_id);
        debug!(%task_id, "grpc: subscribing to task log stream");

        let domain_stream = self
            .handler
            .stream_task_logs(&task_id)
            .await
            .map_err(Status::from)?;

        let proto_stream = domain_stream.map(|ev| Ok(output_event_to_proto(ev)));
        Ok(Response::new(Box::pin(proto_stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    use async_trait::async_trait;
    use bytes::Bytes;
    use solti_model::{
        OutputChunk, OutputEvent, StreamKind as ModelStreamKind, Task, TaskId, TaskPage, TaskQuery,
        TaskRun, TaskSpec,
    };

    use crate::error::ApiError;
    use crate::handler::{ApiHandler, OutputEventStream};

    struct StreamMock;

    #[async_trait]
    impl ApiHandler for StreamMock {
        async fn submit_task(&self, _spec: TaskSpec) -> Result<TaskId, ApiError> {
            unreachable!()
        }
        async fn get_task_status(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
            unreachable!()
        }
        async fn query_tasks(&self, _q: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
            unreachable!()
        }
        async fn list_task_runs(&self, _id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
            unreachable!()
        }
        async fn delete_task(&self, _id: &TaskId) -> Result<(), ApiError> {
            unreachable!()
        }
        async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
            if id.as_str() == "missing" {
                return Err(ApiError::TaskNotFound(id.to_string()));
            }
            let events = vec![
                OutputEvent::RunStarted {
                    attempt: 1,
                    started_at: UNIX_EPOCH + Duration::from_millis(1000),
                },
                OutputEvent::Chunk(OutputChunk {
                    attempt: 1,
                    stream: ModelStreamKind::Stdout,
                    seq: 0,
                    ts: UNIX_EPOCH + Duration::from_millis(1100),
                    line: Bytes::from_static(b"hello-grpc"),
                }),
                OutputEvent::RunFinished {
                    attempt: 1,
                    exit_code: Some(0),
                    finished_at: UNIX_EPOCH + Duration::from_millis(1500),
                },
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn service() -> TaskApiService<StreamMock> {
        TaskApiService::new(Arc::new(StreamMock))
    }

    #[tokio::test]
    async fn stream_task_logs_returns_three_proto_events_in_order() {
        let svc = service();
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            task_id: "tsk_1".into(),
        });

        let response = svc.stream_task_logs(req).await.expect("stream Ok");
        let mut stream = response.into_inner();

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunStarted(r) => {
                assert_eq!(r.attempt, 1);
                assert_eq!(r.started_at, 1000);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::Chunk(c) => {
                assert_eq!(c.attempt, 1);
                assert_eq!(c.stream, proto_api::OutputStreamKind::Stdout as i32);
                assert_eq!(c.seq, 0);
                assert_eq!(&c.line[..], b"hello-grpc");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunFinished(r) => {
                assert_eq!(r.attempt, 1);
                assert_eq!(r.exit_code, Some(0));
                assert_eq!(r.finished_at, 1500);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert!(stream.next().await.is_none(), "stream must terminate");
    }

    #[tokio::test]
    async fn stream_task_logs_rejects_empty_task_id() {
        let svc = service();
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            task_id: "  ".into(),
        });
        let status = match svc.stream_task_logs(req).await {
            Err(s) => s,
            Ok(_) => panic!("expected error status"),
        };
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn stream_task_logs_maps_task_not_found_to_not_found_status() {
        let svc = service();
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            task_id: "missing".into(),
        });
        let status = match svc.stream_task_logs(req).await {
            Err(s) => s,
            Ok(_) => panic!("expected error status"),
        };
        assert_eq!(status.code(), tonic::Code::NotFound);
    }
}
