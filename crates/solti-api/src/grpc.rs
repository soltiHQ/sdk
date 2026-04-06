use std::sync::Arc;

use tonic::{Request, Response, Status};
use tracing::debug;

use solti_model::TaskQuery;

use crate::error::ApiError;
use crate::handler::ApiHandler;
use crate::proto_api::{self, solti_api_server::SoltiApi};

/// gRPC service implementation.
///
/// This struct wraps an `ApiHandler` and implements the generated `SoltiApi` trait.
pub struct SoltiApiService<H> {
    handler: Arc<H>,
}

impl<H> SoltiApiService<H>
where
    H: ApiHandler,
{
    /// Create a new gRPC service with the given handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self { handler }
    }
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
    }

    async fn get_task_status(
        &self,
        request: Request<proto_api::GetTaskStatusRequest>,
    ) -> Result<Response<proto_api::GetTaskStatusResponse>, Status> {
        let req = request.into_inner();

        let task_id = solti_model::TaskId::from(req.task_id);
        debug!(%task_id, "grpc: getting task status");

        let info = self
            .handler
            .get_task_status(&task_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(proto_api::GetTaskStatusResponse {
            info: info.map(proto_api::TaskInfo::from),
        }))
    }

    async fn list_tasks(
        &self,
        request: Request<proto_api::ListTasksRequest>,
    ) -> Result<Response<proto_api::ListTasksResponse>, Status> {
        let req = request.into_inner();

        let mut query = TaskQuery::new();

        if let Some(slot) = req.slot {
            if slot.trim().is_empty() {
                return Err(Status::invalid_argument("slot cannot be empty"));
            }
            query = query.with_slot(slot);
        }

        if let Some(status_raw) = req.status {
            let status = proto_to_domain_status(status_raw)?;
            query = query.with_status(status);
        }

        if req.limit > 0 {
            query = query.with_limit(req.limit as usize);
        }

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

        let tasks = page
            .items
            .into_iter()
            .map(proto_api::TaskInfo::from)
            .collect();

        Ok(Response::new(proto_api::ListTasksResponse {
            tasks,
            total: page.total as u32,
        }))
    }

    async fn list_task_runs(
        &self,
        request: Request<proto_api::ListTaskRunsRequest>,
    ) -> Result<Response<proto_api::ListTaskRunsResponse>, Status> {
        let req = request.into_inner();

        if req.task_id.trim().is_empty() {
            return Err(Status::invalid_argument("task_id cannot be empty"));
        }

        let task_id = solti_model::TaskId::from(req.task_id);
        debug!(%task_id, "grpc: listing task runs");

        let runs = self
            .handler
            .list_task_runs(&task_id)
            .await
            .map_err(Status::from)?;

        let runs = runs
            .into_iter()
            .map(proto_api::TaskRunInfo::from)
            .collect();

        Ok(Response::new(proto_api::ListTaskRunsResponse { runs }))
    }

    async fn cancel_task(
        &self,
        request: Request<proto_api::CancelTaskRequest>,
    ) -> Result<Response<proto_api::CancelTaskResponse>, Status> {
        let req = request.into_inner();

        if req.task_id.trim().is_empty() {
            return Err(Status::invalid_argument("task_id cannot be empty"));
        }

        let task_id = solti_model::TaskId::from(req.task_id);

        self.handler
            .cancel_task(&task_id)
            .await
            .map_err(Status::from)?;

        debug!(%task_id, "grpc: task canceled");
        Ok(Response::new(proto_api::CancelTaskResponse {}))
    }

    async fn delete_task(
        &self,
        request: Request<proto_api::DeleteTaskRequest>,
    ) -> Result<Response<proto_api::DeleteTaskResponse>, Status> {
        let req = request.into_inner();

        if req.task_id.trim().is_empty() {
            return Err(Status::invalid_argument("task_id cannot be empty"));
        }

        let task_id = solti_model::TaskId::from(req.task_id);
        debug!(%task_id, "grpc: deleting task");

        self.handler
            .delete_task(&task_id)
            .await
            .map_err(Status::from)?;

        debug!(%task_id, "grpc: task deleted");
        Ok(Response::new(proto_api::DeleteTaskResponse {}))
    }
}

/// Convert proto TaskStatus i32 to domain TaskPhase.
#[allow(clippy::result_large_err)]
fn proto_to_domain_status(raw: i32) -> Result<solti_model::TaskPhase, Status> {
    let status = proto_api::TaskStatus::try_from(raw)
        .map_err(|_| Status::invalid_argument("invalid status"))?;

    match status {
        proto_api::TaskStatus::Pending => Ok(solti_model::TaskPhase::Pending),
        proto_api::TaskStatus::Running => Ok(solti_model::TaskPhase::Running),
        proto_api::TaskStatus::Succeeded => Ok(solti_model::TaskPhase::Succeeded),
        proto_api::TaskStatus::Failed => Ok(solti_model::TaskPhase::Failed),
        proto_api::TaskStatus::Timeout => Ok(solti_model::TaskPhase::Timeout),
        proto_api::TaskStatus::Canceled => Ok(solti_model::TaskPhase::Canceled),
        proto_api::TaskStatus::Exhausted => Ok(solti_model::TaskPhase::Exhausted),
        proto_api::TaskStatus::Unspecified => {
            Err(Status::invalid_argument("status cannot be unspecified"))
        }
    }
}
