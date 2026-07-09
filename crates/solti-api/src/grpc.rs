//! # gRPC transport.
//!
//! [`TaskApiService`] implements the generated `TaskService` trait from `proto/solti/task/v1/api.proto`, delegating to an [`ApiHandler`](crate::ApiHandler).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use tokio_stream::StreamExt;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use tracing::debug;

use solti_model::{TaskQuery, Token};

use crate::auth::{assert_auth_token_not_empty, bearer_value};
use crate::convert::{output_event_to_proto, proto_to_domain_phase, tasks_page_to_proto};
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
        // Guard, not paired calls: a client hang-up drops this future at the
        // `.await` below, and the `-1` half of a paired decrement would never
        // run, drifting the gauge upward forever.
        let _in_flight = InFlightGuard::enter(&self.metrics);
        let start = Instant::now();
        let result = fut.await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = match &result {
            Ok(_) => 0u16,
            Err(s) => s.code() as u16,
        };
        // The latency/status metric intentionally covers only RPCs that ran to
        // completion (`record_request` is documented as "record a completed
        // request"): a cancelled RPC has neither a final status nor a full
        // duration, so it adjusts the in-flight gauge and nothing else.
        let path = format!("/solti.task.v1.TaskService/{}", method);
        self.metrics
            .record_request(Transport::Grpc, method, &path, status, duration_ms);
        result
    }
}

/// RAII guard for the gRPC in-flight gauge.
///
/// Records `+1` on construction and `-1` in `Drop`, so the gauge stays balanced
/// even when the RPC future is cancelled mid-flight (tonic drops the handler
/// future as soon as the client goes away).
struct InFlightGuard {
    metrics: ApiMetricsHandle,
}

impl InFlightGuard {
    fn enter(metrics: &ApiMetricsHandle) -> Self {
        metrics.record_in_flight_delta(Transport::Grpc, 1);
        Self {
            metrics: Arc::clone(metrics),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.record_in_flight_delta(Transport::Grpc, -1);
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

/// gRPC interceptor enforcing a bearer token on every call.
///
/// Verifies `authorization: Bearer <token>` metadata in constant time and rejects with `Unauthenticated` otherwise.
///
/// This is the same shared secret the agent presents to the control plane in discovery.
/// One config value enables both directions.
/// Orthogonal to TLS. Install via [`build_grpc_server_with_auth`] / [`build_grpc_server_with_metrics_auth`].
#[derive(Clone)]
pub struct BearerAuth {
    expected: Token,
}

impl Interceptor for BearerAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let ok = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_value)
            .map(|presented| self.expected.verify(presented))
            .unwrap_or(false);

        if ok {
            Ok(request)
        } else {
            Err(Status::unauthenticated("missing or invalid bearer token"))
        }
    }
}

/// Like [`build_grpc_server`] but enforcing a bearer token on every call.
///
/// ## Panics
///
/// Panics when `token` is empty — see [`build_grpc_server_with_metrics_auth`].
pub fn build_grpc_server_with_auth<H>(
    handler: Arc<H>,
    token: Token,
) -> InterceptedService<TaskServiceServer<TaskApiService<H>>, BearerAuth>
where
    H: ApiHandler,
{
    build_grpc_server_with_metrics_auth(handler, noop_api_metrics(), token)
}

/// Like [`build_grpc_server_with_metrics`] but enforcing a bearer token.
///
/// Wraps the configured server (message-size limits preserved) in an [`InterceptedService`] that gates every call on the token.
///
/// ## Panics
///
/// Panics when `token` is empty: an empty shared secret would accept an empty
/// bearer credential (`authorization: Bearer `), silently disabling authentication.
pub fn build_grpc_server_with_metrics_auth<H>(
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
    token: Token,
) -> InterceptedService<TaskServiceServer<TaskApiService<H>>, BearerAuth>
where
    H: ApiHandler,
{
    assert_auth_token_not_empty(&token);
    InterceptedService::new(
        build_grpc_server_with_metrics(handler, metrics),
        BearerAuth { expected: token },
    )
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

            if let Some(phase_raw) = req.phase {
                let phase = proto_to_domain_phase(phase_raw).map_err(Status::from)?;
                query = query.with_status(phase);
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

    // --- BearerAuth interceptor ---------------------------------------------

    fn auth_interceptor(secret: &str) -> BearerAuth {
        BearerAuth {
            expected: Token::new(secret),
        }
    }

    fn request_with_authorization(value: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", value.parse().expect("ascii metadata"));
        req
    }

    #[test]
    fn bearer_auth_rejects_missing_metadata() {
        let mut auth = auth_interceptor("sekret");
        let status = auth.call(Request::new(())).unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_auth_rejects_wrong_token() {
        let mut auth = auth_interceptor("sekret");
        let status = auth
            .call(request_with_authorization("Bearer not-the-secret"))
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_auth_rejects_credential_without_scheme() {
        let mut auth = auth_interceptor("sekret");
        let status = auth.call(request_with_authorization("sekret")).unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_auth_rejects_non_bearer_scheme() {
        let mut auth = auth_interceptor("sekret");
        let status = auth
            .call(request_with_authorization("Basic sekret"))
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_auth_accepts_valid_token_scheme_case_insensitively() {
        for header in ["Bearer sekret", "bearer sekret", "BEARER sekret"] {
            let mut auth = auth_interceptor("sekret");
            assert!(
                auth.call(request_with_authorization(header)).is_ok(),
                "header {header:?} must pass"
            );
        }
    }

    #[test]
    #[should_panic(expected = "auth token must not be empty")]
    fn build_grpc_server_with_auth_panics_on_empty_token() {
        let _ = build_grpc_server_with_auth(Arc::new(StreamMock), Token::new(""));
    }

    // --- instrument() metrics ------------------------------------------------

    #[derive(Debug, Default)]
    struct GaugeProbe {
        in_flight: std::sync::atomic::AtomicI64,
        completed: std::sync::atomic::AtomicUsize,
    }

    impl crate::metrics::ApiMetricsBackend for GaugeProbe {
        fn record_request(
            &self,
            _transport: crate::metrics::Transport,
            _method: &str,
            _path: &str,
            _status: u16,
            _duration_ms: u64,
        ) {
            self.completed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn record_in_flight_delta(&self, _transport: crate::metrics::Transport, delta: i64) {
            self.in_flight
                .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn probed_service() -> (Arc<GaugeProbe>, TaskApiService<StreamMock>) {
        let probe = Arc::new(GaugeProbe::default());
        let handle: ApiMetricsHandle = probe.clone();
        (
            probe,
            TaskApiService::new_with_metrics(Arc::new(StreamMock), handle),
        )
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

    #[test]
    fn in_flight_gauge_recovers_when_rpc_future_is_dropped() {
        use std::future::Future;
        use std::sync::atomic::Ordering;
        use std::task::{Context, Poll, Waker};

        let (probe, svc) = probed_service();

        // Never-completing RPC body: models a handler still working when the
        // client hangs up and tonic drops the whole future.
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
}
