//! # gRPC transport.
//!
//! [`SoltiApiService`] implements the generated `SoltiApi` trait from `proto/solti/v1/api.proto`,
//! delegating to an [`ApiHandler`](crate::ApiHandler).
//!
//! Per-request metrics are recorded via [`ApiMetricsHandle`](crate::ApiMetricsHandle); pass a
//! real backend through [`SoltiApiService::new_with_metrics`] or
//! [`build_grpc_server_with_metrics`] to observe requests.

use std::sync::Arc;
use std::time::Instant;

use tonic::{Request, Response, Status};
use tracing::debug;

use solti_model::TaskQuery;

use crate::convert::{proto_to_domain_status, tasks_page_to_proto};
use crate::error::ApiError;
use crate::handler::ApiHandler;
use crate::metrics::{ApiMetricsHandle, Transport, noop_api_metrics};
use crate::proto_api::{self, solti_api_server::SoltiApi, solti_api_server::SoltiApiServer};
use crate::validate::{clamp_list_limit, non_empty_id};

/// gRPC service wrapping an [`ApiHandler`](crate::ApiHandler).
///
/// ## Also
///
/// - `SoltiApiServer` generated tonic server wrapper.
/// - [`ApiError`](crate::ApiError) mapped to `tonic::Status`.
pub struct SoltiApiService<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
}

impl<H> SoltiApiService<H>
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
        F: std::future::Future<Output = Result<Response<T>, Status>>,
    {
        self.metrics.record_in_flight_delta(Transport::Grpc, 1);
        let start = Instant::now();
        let result = fut.await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = match &result {
            Ok(_) => 0u16,
            Err(s) => s.code() as u16,
        };
        let path = format!("/solti.v1.SoltiApi/{}", method);
        self.metrics
            .record_request(Transport::Grpc, method, &path, status, duration_ms);
        self.metrics.record_in_flight_delta(Transport::Grpc, -1);
        result
    }
}

/// Build a configured `SoltiApiServer` with no-op metrics.
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
pub fn build_grpc_server<H>(handler: Arc<H>) -> SoltiApiServer<SoltiApiService<H>>
where
    H: ApiHandler,
{
    build_grpc_server_with_metrics(handler, noop_api_metrics())
}

/// Build a configured `SoltiApiServer` with an explicit metrics backend.
pub fn build_grpc_server_with_metrics<H>(
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
) -> SoltiApiServer<SoltiApiService<H>>
where
    H: ApiHandler,
{
    SoltiApiServer::new(SoltiApiService::new_with_metrics(handler, metrics))
        .max_decoding_message_size(crate::MAX_REQUEST_BYTES)
        .max_encoding_message_size(crate::MAX_REQUEST_BYTES)
}

#[tonic::async_trait]
impl<H> SoltiApi for SoltiApiService<H>
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
}
