use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::error::ApiError;
use crate::handler::ApiHandler;
use crate::proto::{self, tno_api_server::TnoApi};

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
        request: Request<proto::SubmitTaskRequest>,
    ) -> Result<Response<proto::SubmitTaskResponse>, Status> {
        let req = request.into_inner();

        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("missing spec"))?;

        let spec = tno_model::CreateSpec::try_from(spec).map_err(|e: ApiError| Status::from(e))?;

        let task_id = self.handler.submit_task(spec).await.map_err(Status::from)?;

        Ok(Response::new(proto::SubmitTaskResponse {
            task_id: task_id.to_string(),
        }))
    }

    async fn get_task_status(
        &self,
        request: Request<proto::GetTaskStatusRequest>,
    ) -> Result<Response<proto::GetTaskStatusResponse>, Status> {
        let req = request.into_inner();

        let task_id = tno_model::TaskId::from(req.task_id);

        let info = self
            .handler
            .get_task_status(&task_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(proto::GetTaskStatusResponse {
            info: info.map(proto::TaskInfo::from),
        }))
    }
}
