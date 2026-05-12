//! # Supervisor adapter.
//!
//! [`SupervisorApiAdapter`] bridges [`SupervisorApi`](solti_core::SupervisorApi) to [`ApiHandler`].

use std::sync::Arc;

use async_trait::async_trait;
use solti_core::SupervisorApi;
use solti_model::{OutputEvent, Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use crate::error::ApiError;
use crate::handler::{ApiHandler, OutputEventStream};

/// Adapter that bridges [`SupervisorApi`] to [`ApiHandler`].
///
/// Ready-to-use implementation that directly delegates to `SupervisorApi`.
///
/// ## Also
///
/// - [`ApiHandler`] the trait this adapter implements.
/// - [`ApiError::Core`] wraps `CoreError` from the supervisor.
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

    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError> {
        self.supervisor
            .delete_task(id)
            .await
            .map_err(ApiError::from)
    }

    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
        let receiver = self
            .supervisor
            .output_registry()
            .subscribe(id)
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))?;

        let stream = BroadcastStream::new(receiver).map(|res| {
            res.unwrap_or_else(
                |BroadcastStreamRecvError::Lagged(skipped)| OutputEvent::Lagged { skipped },
            )
        });
        Ok(Box::pin(stream))
    }
}
