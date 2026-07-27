//! # Supervisor adapter.
//!
//! [`SupervisorApiAdapter`] bridges [`SupervisorApi`](solti_core::SupervisorApi) to [`ApiHandler`].

use std::sync::Arc;

use async_trait::async_trait;
use solti_core::{CollectionError, CoreError, SupervisorApi, WritePreconditionViolation};
use solti_model::{
    OutputEvent, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRun,
    WritePreconditions,
};
use tokio_stream::StreamExt;

use crate::error::{ApiConflict, ApiError, ApiErrorCause};
use crate::handler::{ApiHandler, OutputEventStream, TaskWatchEventStream};
use crate::visibility::{manifest_is_visible, task_is_visible, workload_is_visible};

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

    /// Keep an opened wire stream pinned to the generation that was visible
    /// when the subscription was created.
    fn event_is_from_generation(event: &OutputEvent, generation: u64) -> bool {
        match event {
            OutputEvent::Chunk(chunk) => chunk.generation == generation,
            OutputEvent::RunStarted {
                generation: event_generation,
                ..
            }
            | OutputEvent::RunFinished {
                generation: event_generation,
                ..
            } => *event_generation == generation,
            OutputEvent::Lagged { .. } => true,
            _ => false,
        }
    }
}

#[async_trait]
impl ApiHandler for SupervisorApiAdapter {
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        if !manifest_is_visible(&manifest) {
            return Err(ApiError::InvalidRequest(
                "workload has no public wire representation".into(),
            ));
        }
        self.supervisor
            .create_task(manifest)
            .await
            .map_err(map_core_error)
    }

    async fn apply_task(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        if !manifest_is_visible(&manifest) {
            return Err(ApiError::InvalidRequest(
                "workload has no public wire representation".into(),
            ));
        }
        self.supervisor
            .apply_task_where(manifest, preconditions, task_is_visible)
            .await
            .map_err(map_core_error)
    }

    async fn get_task(&self, name: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(self.supervisor.get_task(name).filter(task_is_visible))
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        self.supervisor
            .query_tasks_where(&query, task_is_visible)
            .map_err(map_collection_error)
    }

    async fn watch_tasks(
        &self,
        filter: TaskFilter,
        resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        let stream = self
            .supervisor
            .watch_tasks_where(&filter, resource_version.as_deref(), task_is_visible)
            .map_err(map_collection_error)?;
        Ok(Box::pin(
            stream.map(|event| event.map_err(map_collection_error)),
        ))
    }

    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
        self.supervisor
            .list_task_runs_where(id, workload_is_visible)
            .await
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))
    }

    async fn delete_task(
        &self,
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        self.supervisor
            .delete_task_where(id, preconditions, task_is_visible)
            .await
            .map_err(map_core_error)
    }

    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
        let (generation, stream) = self
            .supervisor
            .subscribe_output_where(id, task_is_visible)
            .await
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))?;
        Ok(Box::pin(stream.filter(move |event| {
            Self::event_is_from_generation(event, generation)
        })))
    }
}

fn map_core_error(error: CoreError) -> ApiError {
    match error {
        CoreError::InvalidSpec(inner) => ApiError::InvalidRequest(inner.to_string()),
        CoreError::AlreadyExists(message) => ApiError::AlreadyExists(message),
        CoreError::NotFound(message) => ApiError::TaskNotFound(message),
        CoreError::Conflict(conflict) => {
            let causes = conflict
                .violations()
                .iter()
                .map(|violation| match violation {
                    WritePreconditionViolation::Uid { .. } => {
                        ApiErrorCause::new("UIDMismatch", violation.to_string())
                            .with_field("preconditions.uid")
                    }
                    WritePreconditionViolation::ResourceVersion { .. } => {
                        ApiErrorCause::new("ResourceVersionMismatch", violation.to_string())
                            .with_field("preconditions.resourceVersion")
                    }
                    _ => ApiErrorCause::new("PreconditionFailed", violation.to_string()),
                })
                .collect();
            ApiError::Conflict(ApiConflict::new(conflict.name().to_string(), causes))
        }
        CoreError::ShuttingDown => ApiError::Unavailable("supervisor is shutting down".into()),
        other => ApiError::Internal(other.to_string()),
    }
}

fn map_collection_error(error: CollectionError) -> ApiError {
    match &error {
        CollectionError::InvalidResourceVersion { .. }
        | CollectionError::ContinuationFilterMismatch
        | CollectionError::ContinuationCursorNotFound { .. } => {
            ApiError::InvalidRequest(error.to_string())
        }
        CollectionError::ResourceVersionExpired { .. } => {
            ApiError::ResourceVersionExpired(error.to_string())
        }
        _ => ApiError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    use bytes::Bytes;
    use solti_model::{
        EmbeddedSpec, ExtensionWorkload, OutputChunk, StreamKind, TaskSpec, TaskWorkload,
        WORKLOAD_API_VERSION, WorkloadTypeMeta,
    };
    use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
    use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

    struct TestRunner;

    impl Runner for TestRunner {
        fn name(&self) -> &str {
            "adapter-test"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            vec![
                WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess")
                    .expect("built-in workload GVK"),
                WorkloadTypeMeta::new("workloads.example.io/v1", "ExampleJob")
                    .expect("extension workload GVK"),
            ]
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _context: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                Ok::<(), TaskError>(())
            }))
        }
    }

    async fn supervisor() -> SupervisorApi {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(TestRunner)).unwrap();
        SupervisorApi::builder(router)
            .start()
            .await
            .expect("SupervisorApiBuilder::start")
    }

    #[tokio::test]
    async fn get_task_hides_embedded_tasks() {
        let api = supervisor().await;

        // Embedded tasks enter state through the prebuilt-task SDK path; the
        // wire path can never create them, but a point GET by name
        // used to reach them via `state.get` and fail the proto conversion.
        let task: TaskRef = TaskFn::arc("embedded-probe", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let spec = TaskSpec::builder(
            "slot-embedded",
            TaskWorkload::Embedded(EmbeddedSpec::new("adapter-test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .expect("spec builds");
        let manifest = TaskManifest::new("embedded-probe", spec).expect("manifest builds");
        let created = api
            .create_embedded_task(manifest, task)
            .await
            .expect("create_embedded_task");
        let name = created.name().clone();

        assert!(
            api.get_task(&name).is_some(),
            "supervisor state must still hold the embedded task"
        );

        let adapter = SupervisorApiAdapter::new(Arc::new(api));
        let visible = adapter
            .get_task(&name)
            .await
            .expect("get_task must not fail");
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
        let spec = solti_model::TaskSpec::builder(
            "slot-embedded-guard",
            TaskWorkload::Embedded(EmbeddedSpec::new("adapter-test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .expect("spec builds");
        let manifest = TaskManifest::new("embedded-guard", spec).expect("manifest builds");
        let created = api
            .create_embedded_task(manifest, task)
            .await
            .expect("create_embedded_task");
        let name = created.name().clone();

        let adapter = SupervisorApiAdapter::new(Arc::new(api));

        let runs = adapter.list_task_runs(&name).await;
        assert!(
            matches!(runs, Err(ApiError::TaskNotFound(_))),
            "embedded run history must look like an unknown id, got {runs:?}"
        );

        let deleted = adapter.delete_task(&name, WritePreconditions::new()).await;
        assert!(
            matches!(deleted, Err(ApiError::TaskNotFound(_))),
            "deleting an embedded task must look like an unknown id, got {deleted:?}"
        );
        assert!(
            adapter.supervisor.get_task(&name).is_some(),
            "the embedded task must survive the delete attempt"
        );

        let stream = adapter.stream_task_logs(&name).await;
        assert!(
            matches!(stream, Err(ApiError::TaskNotFound(_))),
            "embedded log streams must look like an unknown id"
        );
    }

    #[tokio::test]
    async fn get_task_unknown_name_is_none() {
        let adapter = SupervisorApiAdapter::new(Arc::new(supervisor().await));
        let visible = adapter
            .get_task(&TaskId::new("no-such-task").unwrap())
            .await
            .expect("get_task must not fail");
        assert!(visible.is_none());
    }

    #[tokio::test]
    async fn query_filters_sdk_only_workloads_before_pagination() {
        use solti_model::{
            Flag, LabelSelector, Labels, SubprocessMode, SubprocessSpec, TaskEnv, TaskQuery,
        };

        let api = supervisor().await;
        let hidden_ref: TaskRef = TaskFn::arc("hidden-runtime", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let hidden_spec = solti_model::TaskSpec::builder(
            "hidden-slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("adapter-test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .unwrap();
        let mut hidden_labels = Labels::new();
        hidden_labels.insert("environment", "production");
        api.create_embedded_task(
            TaskManifest::new("aaa-hidden", hidden_spec)
                .unwrap()
                .with_labels(hidden_labels)
                .unwrap(),
            hidden_ref,
        )
        .await
        .unwrap();

        let visible_workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: Vec::new(),
            },
            TaskEnv::new(),
            None,
            Flag::from(true),
        ));
        let visible_spec =
            solti_model::TaskSpec::builder("visible-slot", visible_workload, 5_000_u64)
                .build()
                .unwrap();
        let mut visible_labels = Labels::new();
        visible_labels.insert("environment", "production");
        api.create_task(
            TaskManifest::new("zzz-visible", visible_spec)
                .unwrap()
                .with_labels(visible_labels)
                .unwrap(),
        )
        .await
        .unwrap();

        let adapter = SupervisorApiAdapter::new(Arc::new(api));
        let selector: LabelSelector = "environment=production".parse().unwrap();
        let page = adapter
            .query_tasks(
                TaskQuery::new()
                    .with_label_selector(selector)
                    .unwrap()
                    .with_limit(1),
            )
            .await
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].name().as_str(), "zzz-visible");
        assert!(page.continuation.is_none());
        assert_eq!(page.remaining_item_count, 0);
    }

    #[tokio::test]
    async fn apply_cannot_replace_an_embedded_task_through_the_public_api() {
        use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv};

        let api = supervisor().await;
        let hidden_ref: TaskRef = TaskFn::arc("hidden-runtime", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let hidden_spec = TaskSpec::builder(
            "hidden-slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("adapter-test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .unwrap();
        api.create_embedded_task(
            TaskManifest::new("hidden-apply-target", hidden_spec).unwrap(),
            hidden_ref,
        )
        .await
        .unwrap();

        let visible_spec = TaskSpec::builder(
            "visible-slot",
            TaskWorkload::Subprocess(SubprocessSpec::new(
                SubprocessMode::Command {
                    command: "true".into(),
                    args: Vec::new(),
                },
                TaskEnv::new(),
                None,
                Flag::from(true),
            )),
            5_000_u64,
        )
        .build()
        .unwrap();
        let adapter = SupervisorApiAdapter::new(Arc::new(api));
        let result = adapter
            .apply_task(
                TaskManifest::new("hidden-apply-target", visible_spec).unwrap(),
                WritePreconditions::new(),
            )
            .await;

        assert!(matches!(result, Err(ApiError::TaskNotFound(_))));
        let stored = adapter
            .supervisor
            .get_task(&TaskId::new("hidden-apply-target").unwrap())
            .expect("embedded task remains stored");
        assert!(matches!(
            stored.spec().workload(),
            TaskWorkload::Embedded(_)
        ));
    }

    #[tokio::test]
    async fn extension_workloads_are_public_and_routable() {
        let api = supervisor().await;
        let adapter = SupervisorApiAdapter::new(Arc::new(api));
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "workloads.example.io/v1",
                "ExampleJob",
                serde_json::json!({"value": 9_007_199_254_740_993_u64}),
            )
            .unwrap(),
        );
        let spec = TaskSpec::builder("extension-slot", workload, 5_000_u64)
            .build()
            .unwrap();

        let created = adapter
            .create_task(TaskManifest::new("extension-task", spec).unwrap())
            .await
            .expect("extension workload must be accepted");
        assert_eq!(created.status().phase(), solti_model::TaskPhase::Pending);
        assert_eq!(created.status().observed_generation(), 0);
        let fetched = adapter
            .get_task(created.name())
            .await
            .expect("get succeeds")
            .expect("extension task remains visible");

        assert!(matches!(
            fetched.spec().workload(),
            TaskWorkload::Extension(_)
        ));
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

        let shutting_down = map_core_error(CoreError::ShuttingDown);
        assert!(matches!(shutting_down, ApiError::Unavailable(_)));

        let internal = map_core_error(CoreError::Mapping("mapping".into()));
        assert!(matches!(internal, ApiError::Internal(message) if message.contains("mapping")));
    }

    #[test]
    fn collection_errors_translate_to_api_owned_categories() {
        let invalid = map_collection_error(CollectionError::InvalidResourceVersion {
            resource_version: "bad".into(),
        });
        assert!(matches!(invalid, ApiError::InvalidRequest(_)));

        let mismatch = map_collection_error(CollectionError::ContinuationFilterMismatch);
        assert!(matches!(mismatch, ApiError::InvalidRequest(_)));

        let missing_cursor = map_collection_error(CollectionError::ContinuationCursorNotFound {
            name: TaskId::new("missing").unwrap(),
        });
        assert!(matches!(missing_cursor, ApiError::InvalidRequest(_)));

        let expired = map_collection_error(CollectionError::ResourceVersionExpired {
            resource_version: "old:1".into(),
        });
        assert!(matches!(expired, ApiError::ResourceVersionExpired(_)));
    }

    #[test]
    fn output_stream_is_pinned_to_the_opened_generation() {
        let current = OutputEvent::Chunk(OutputChunk {
            generation: 7,
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(b"current"),
        });
        let stale = OutputEvent::RunStarted {
            generation: 6,
            attempt: 1,
            started_at: UNIX_EPOCH,
        };
        let future = OutputEvent::RunFinished {
            generation: 8,
            attempt: 1,
            exit_code: Some(0),
            finished_at: UNIX_EPOCH,
        };
        let lagged = OutputEvent::Lagged { skipped: 2 };

        assert!(SupervisorApiAdapter::event_is_from_generation(&current, 7));
        assert!(!SupervisorApiAdapter::event_is_from_generation(&stale, 7));
        assert!(!SupervisorApiAdapter::event_is_from_generation(&future, 7));
        assert!(SupervisorApiAdapter::event_is_from_generation(&lagged, 7));
    }
}
