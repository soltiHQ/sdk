use std::sync::Arc;

use async_trait::async_trait;
use solti_core::SupervisorApi;
use solti_model::{Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec};

use crate::error::ApiError;
use crate::handler::ApiHandler;

/// Adapter that bridges [`SupervisorApi`] to [`ApiHandler`].
///
/// Ready-to-use implementation that directly delegates to `SupervisorApi`.
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
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError> {
        self.supervisor.submit(&spec).await.map_err(ApiError::from)
    }

    async fn get_task_status(&self, id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(self.supervisor.get_task(id))
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(self.supervisor.query_tasks(&query))
    }

    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        Ok(self.supervisor.list_task_runs(id))
    }

    async fn cancel_task(&self, id: &TaskId) -> Result<(), ApiError> {
        self.supervisor
            .cancel_task(id)
            .await
            .map_err(ApiError::from)
    }

    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError> {
        if self.supervisor.delete_task(id) {
            Ok(())
        } else {
            Err(ApiError::TaskNotFound(id.to_string()))
        }
    }
}
