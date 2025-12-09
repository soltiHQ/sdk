use std::sync::Arc;

use async_trait::async_trait;
use tno_core::SupervisorApi;
use tno_model::{CreateSpec, TaskId, TaskInfo};

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
}
