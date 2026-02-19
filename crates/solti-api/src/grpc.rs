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
            solti_model::CreateSpec::try_from(spec).map_err(|e: ApiError| Status::from(e))?;

        debug!(slot = %spec.slot, kind = ?spec.kind, "grpc: submitting task");
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

    async fn list_all_tasks(
        &self,
        _request: Request<proto_api::ListAllTasksRequest>,
    ) -> Result<Response<proto_api::ListAllTasksResponse>, Status> {
        let tasks = self.handler.list_all_tasks().await.map_err(Status::from)?;
        debug!(count = tasks.len(), "grpc: tasks listed");

        let tasks = tasks.into_iter().map(proto_api::TaskInfo::from).collect();

        Ok(Response::new(proto_api::ListAllTasksResponse { tasks }))
    }

    async fn list_tasks_by_slot(
        &self,
        request: Request<proto_api::ListTasksBySlotRequest>,
    ) -> Result<Response<proto_api::ListTasksBySlotResponse>, Status> {
        let req = request.into_inner();

        if req.slot.trim().is_empty() {
            return Err(Status::invalid_argument("slot cannot be empty"));
        }

        debug!(slot = %req.slot, "grpc: listing tasks by slot");
        let tasks = self
            .handler
            .list_tasks_by_slot(&req.slot)
            .await
            .map_err(Status::from)?;

        let tasks = tasks.into_iter().map(proto_api::TaskInfo::from).collect();

        Ok(Response::new(proto_api::ListTasksBySlotResponse { tasks }))
    }

    async fn list_tasks_by_status(
        &self,
        request: Request<proto_api::ListTasksByStatusRequest>,
    ) -> Result<Response<proto_api::ListTasksByStatusResponse>, Status> {
        let req = request.into_inner();

        let domain_status = proto_to_domain_status(req.status)?;

        let tasks = self
            .handler
            .list_tasks_by_status(domain_status)
            .await
            .map_err(Status::from)?;

        let tasks = tasks.into_iter().map(proto_api::TaskInfo::from).collect();

        Ok(Response::new(proto_api::ListTasksByStatusResponse {
            tasks,
        }))
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
}

/// Convert proto TaskStatus i32 to domain TaskStatus.
#[allow(clippy::result_large_err)]
fn proto_to_domain_status(raw: i32) -> Result<solti_model::TaskStatus, Status> {
    let status = proto_api::TaskStatus::try_from(raw)
        .map_err(|_| Status::invalid_argument("invalid status"))?;

    match status {
        proto_api::TaskStatus::Pending => Ok(solti_model::TaskStatus::Pending),
        proto_api::TaskStatus::Running => Ok(solti_model::TaskStatus::Running),
        proto_api::TaskStatus::Succeeded => Ok(solti_model::TaskStatus::Succeeded),
        proto_api::TaskStatus::Failed => Ok(solti_model::TaskStatus::Failed),
        proto_api::TaskStatus::Timeout => Ok(solti_model::TaskStatus::Timeout),
        proto_api::TaskStatus::Canceled => Ok(solti_model::TaskStatus::Canceled),
        proto_api::TaskStatus::Exhausted => Ok(solti_model::TaskStatus::Exhausted),
        proto_api::TaskStatus::Unspecified => {
            Err(Status::invalid_argument("status cannot be unspecified"))
        }
    }
}
