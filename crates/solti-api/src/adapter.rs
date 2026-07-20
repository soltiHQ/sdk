//! # Supervisor adapter.
//!
//! [`SupervisorApiAdapter`] bridges [`SupervisorApi`](solti_core::SupervisorApi) to [`ApiHandler`].

use std::sync::Arc;

use async_trait::async_trait;
use solti_core::{CoreError, SupervisorApi};
use solti_model::{Task, TaskId, TaskKind, TaskPage, TaskQuery, TaskRun, TaskSpec};

use crate::error::ApiError;
use crate::handler::{ApiHandler, OutputEventStream};

/// Adapter that bridges [`SupervisorApi`] to [`ApiHandler`].
///
/// Ready-to-use implementation that directly delegates to `SupervisorApi`.
///
/// ## Also
///
/// - [`ApiHandler`] the trait this adapter implements.
/// - [`ApiError`] receives API-owned categories translated from [`CoreError`].
pub struct SupervisorApiAdapter {
    supervisor: Arc<SupervisorApi>,
}

impl SupervisorApiAdapter {
    /// Create a new adapter wrapping the given supervisor.
    pub fn new(supervisor: Arc<SupervisorApi>) -> Self {
        Self { supervisor }
    }

    /// Embedded tasks live in supervisor state but have no wire representation
    /// (`TaskData::try_from` rejects them). Every per-id operation treats them
    /// as absent, so agent-internal tasks can neither be observed nor deleted
    /// through the API.
    fn hidden_from_wire(&self, id: &TaskId) -> bool {
        self.supervisor
            .get_task(id)
            .is_some_and(|t| matches!(t.spec().kind(), TaskKind::Embedded))
    }
}

#[async_trait]
impl ApiHandler for SupervisorApiAdapter {
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError> {
        self.supervisor.submit(&spec).await.map_err(map_core_error)
    }

    async fn get_task_status(&self, id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(self
            .supervisor
            .get_task(id)
            .filter(|t| !matches!(t.spec().kind(), TaskKind::Embedded)))
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Ok(self.supervisor.query_tasks(&query))
    }

    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        if self.hidden_from_wire(id) {
            return Ok(Vec::new());
        }
        Ok(self.supervisor.list_task_runs(id))
    }

    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError> {
        if self.hidden_from_wire(id) {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        self.supervisor
            .delete_task(id)
            .await
            .map_err(map_core_error)
    }

    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
        if self.hidden_from_wire(id) {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        let stream = self
            .supervisor
            .subscribe_output(id)
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))?;
        Ok(Box::pin(stream))
    }
}

fn map_core_error(error: CoreError) -> ApiError {
    match error {
        CoreError::InvalidSpec(inner) => ApiError::InvalidRequest(inner.to_string()),
        CoreError::AlreadyExists(message) => ApiError::AlreadyExists(message),
        CoreError::NotFound(message) => ApiError::TaskNotFound(message),
        other => ApiError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use solti_core::StateConfig;
    use solti_runner::RunnerRouter;
    use taskvisor::{ControllerConfig, SupervisorConfig, TaskContext, TaskError, TaskFn, TaskRef};

    async fn supervisor() -> SupervisorApi {
        SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new")
    }

    #[tokio::test]
    async fn get_task_status_hides_embedded_tasks() {
        let api = supervisor().await;

        // Embedded tasks enter state only through `submit_with_task` (a pre-built
        // task body); the wire path can never create them, but a point GET by id
        // used to reach them via `state.get` and fail the proto conversion.
        let task: TaskRef = TaskFn::arc("embedded-probe", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let spec = TaskSpec::builder("slot-embedded", TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("spec builds");
        let task_id = api
            .submit_with_task(task, &spec)
            .await
            .expect("submit_with_task");

        assert!(
            api.get_task(&task_id).is_some(),
            "supervisor state must still hold the embedded task"
        );

        let adapter = SupervisorApiAdapter::new(Arc::new(api));
        let visible = adapter
            .get_task_status(&task_id)
            .await
            .expect("get_task_status must not fail");
        assert!(
            visible.is_none(),
            "embedded tasks must be reported as absent over the API"
        );
    }

    #[tokio::test]
    async fn embedded_tasks_are_absent_for_all_per_id_operations() {
        let api = supervisor().await;

        let task: TaskRef = TaskFn::arc("embedded-guard", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let spec = TaskSpec::builder("slot-embedded-guard", TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("spec builds");
        let task_id = api
            .submit_with_task(task, &spec)
            .await
            .expect("submit_with_task");

        let adapter = SupervisorApiAdapter::new(Arc::new(api));

        let runs = adapter
            .list_task_runs(&task_id)
            .await
            .expect("list_task_runs must not fail");
        assert!(runs.is_empty(), "embedded runs must not leak over the API");

        let deleted = adapter.delete_task(&task_id).await;
        assert!(
            matches!(deleted, Err(ApiError::TaskNotFound(_))),
            "deleting an embedded task must look like an unknown id, got {deleted:?}"
        );
        assert!(
            adapter.supervisor.get_task(&task_id).is_some(),
            "the embedded task must survive the delete attempt"
        );

        let stream = adapter.stream_task_logs(&task_id).await;
        assert!(
            matches!(stream, Err(ApiError::TaskNotFound(_))),
            "embedded log streams must look like an unknown id"
        );
    }

    #[tokio::test]
    async fn get_task_status_unknown_id_is_none() {
        let adapter = SupervisorApiAdapter::new(Arc::new(supervisor().await));
        let visible = adapter
            .get_task_status(&TaskId::from("no-such-task"))
            .await
            .expect("get_task_status must not fail");
        assert!(visible.is_none());
    }

    #[test]
    fn core_errors_translate_to_api_owned_categories() {
        let invalid = map_core_error(CoreError::InvalidSpec(solti_model::ModelError::Invalid(
            "bad".into(),
        )));
        assert!(
            matches!(invalid, ApiError::InvalidRequest(message) if message == "invalid model: bad")
        );

        let duplicate = map_core_error(CoreError::AlreadyExists("duplicate".into()));
        assert!(matches!(duplicate, ApiError::AlreadyExists(message) if message == "duplicate"));

        let missing = map_core_error(CoreError::NotFound("missing".into()));
        assert!(matches!(missing, ApiError::TaskNotFound(message) if message == "missing"));

        let internal = map_core_error(CoreError::Mapping("mapping".into()));
        assert!(matches!(internal, ApiError::Internal(message) if message.contains("mapping")));
    }
}
