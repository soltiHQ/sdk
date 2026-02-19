use std::sync::Arc;

use async_trait::async_trait;
use solti_core::SupervisorApi;
use solti_model::{CreateSpec, TaskId, TaskInfo, TaskPage, TaskQuery, TaskStatus};

use crate::error::ApiError;
use crate::handler::ApiHandler;

/// Adapter that bridges `SupervisorApi` to `ApiHandler`.
///
/// This is a ready-to-use implementation that directly delegates to `SupervisorApi`.
pub struct SupervisorApiAdapter {
    supervisor: Arc<SupervisorApi>,
}

impl SupervisorApiAdapter {
    /// Create a new adapter wrapping the given supervisor.
    pub fn new(supervisor: Arc<SupervisorApi>) -> Self {
        Self { supervisor }
    }
}

#[async_trait]
impl ApiHandler for SupervisorApiAdapter {
    async fn submit_task(&self, spec: CreateSpec) -> Result<TaskId, ApiError> {
        self.supervisor.submit(&spec).await.map_err(ApiError::from)
    }

    async fn get_task_status(&self, id: &TaskId) -> Result<Option<TaskInfo>, ApiError> {
        Ok(self.supervisor.get_task(id))
    }

    async fn list_all_tasks(&self) -> Result<Vec<TaskInfo>, ApiError> {
        Ok(self.supervisor.list_all_tasks())
    }

    async fn list_tasks_by_slot(&self, slot: &str) -> Result<Vec<TaskInfo>, ApiError> {
        Ok(self.supervisor.list_tasks_by_slot(slot))
    }

    async fn list_tasks_by_status(&self, status: TaskStatus) -> Result<Vec<TaskInfo>, ApiError> {
        Ok(self.supervisor.list_tasks_by_status(status))
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<TaskInfo>, ApiError> {
        Ok(self.supervisor.query_tasks(&query))
    }

    async fn cancel_task(&self, id: &TaskId) -> Result<(), ApiError> {
        self.supervisor
            .cancel_task(id)
            .await
            .map_err(ApiError::from)
    }
}
