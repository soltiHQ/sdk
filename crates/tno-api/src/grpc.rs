use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::error::ApiError;
use crate::handler::ApiHandler;
use crate::proto_api::{self, tno_api_server::TnoApi};

/// gRPC service implementation.
///
/// This struct wraps an `ApiHandler` and implements the generated `TnoApi` trait.
pub struct TnoApiService<H> {
    handler: Arc<H>,
}

impl<H> TnoApiService<H>
where
    H: ApiHandler,
{
    /// Create a new gRPC service with the given handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self { handler }
    }
}

#[tonic::async_trait]
impl<H> TnoApi for TnoApiService<H>
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

        let spec = tno_model::CreateSpec::try_from(spec).map_err(|e: ApiError| Status::from(e))?;

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

        let task_id = tno_model::TaskId::from(req.task_id);

        let info = self
            .handler
            .get_task_status(&task_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(proto_api::GetTaskStatusResponse {
            info: info.map(proto_api::TaskInfo::from),
        }))
    }

    async fn list_all_tasks(
        &self,
        _request: Request<proto_api::ListAllTasksRequest>,
    ) -> Result<Response<proto_api::ListAllTasksResponse>, Status> {
        let tasks = self.handler.list_all_tasks().await.map_err(Status::from)?;

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

        let status = proto_api::TaskStatus::try_from(req.status)
            .map_err(|_| Status::invalid_argument("invalid status"))?;

        if status == proto_api::TaskStatus::Unspecified {
            return Err(Status::invalid_argument("status cannot be unspecified"));
        }

        let domain_status = match status {
            proto_api::TaskStatus::Pending => tno_model::TaskStatus::Pending,
            proto_api::TaskStatus::Running => tno_model::TaskStatus::Running,
            proto_api::TaskStatus::Succeeded => tno_model::TaskStatus::Succeeded,
            proto_api::TaskStatus::Failed => tno_model::TaskStatus::Failed,
            proto_api::TaskStatus::Timeout => tno_model::TaskStatus::Timeout,
            proto_api::TaskStatus::Canceled => tno_model::TaskStatus::Canceled,
            proto_api::TaskStatus::Exhausted => tno_model::TaskStatus::Exhausted,
            proto_api::TaskStatus::Unspecified => unreachable!(),
        };

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

        let task_id = tno_model::TaskId::from(req.task_id);

        self.handler
            .cancel_task(&task_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(proto_api::CancelTaskResponse {}))
    }
}
