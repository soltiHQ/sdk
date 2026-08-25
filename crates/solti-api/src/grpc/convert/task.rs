//! # Task Conversion
//!
//! Converts create and apply manifests into domain desired state.
//! Converts stored tasks, list pages, and watch events into protobuf responses.

use prost::Message as _;
use solti_model::{
    Annotations, TASK_API_VERSION, TASK_KIND, Task, TaskFilter, TaskManifest, TaskPage,
};

use super::spec::{convert_labels, convert_task_spec, spec_to_proto};
use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<Task> for proto_api::Task {
    type Error = ApiError;

    fn try_from(task: Task) -> Result<Self, Self::Error> {
        let (type_meta, metadata, spec, status) = task.into_parts();
        let (observed_generation, phase, attempt, exit_code, error, conditions) =
            status.into_parts();

        Ok(proto_api::Task {
            api_version: type_meta.api_version().to_owned(),
            kind: type_meta.kind().to_owned(),
            metadata: Some(proto_api::ObjectMeta::try_from(&metadata)?),
            spec: Some(spec_to_proto(&spec)?),
            status: Some(proto_api::TaskStatus {
                observed_generation,
                phase: proto_api::TaskPhase::try_from(phase)? as i32,
                exit_code,
                attempt,
                error,
                conditions: conditions
                    .into_iter()
                    .map(proto_api::TaskCondition::try_from)
                    .collect::<Result<_, _>>()?,
            }),
        })
    }
}

/// Converts a create or apply manifest into domain desired state.
pub(crate) fn task_manifest_from_proto(
    manifest: proto_api::TaskManifest,
) -> Result<TaskManifest, ApiError> {
    if manifest.api_version != TASK_API_VERSION {
        return Err(ApiError::InvalidRequest(format!(
            "Task apiVersion must be `{TASK_API_VERSION}`",
        )));
    }
    if manifest.kind != TASK_KIND {
        return Err(ApiError::InvalidRequest(format!(
            "Task kind must be `{TASK_KIND}`",
        )));
    }
    let metadata = manifest
        .metadata
        .ok_or_else(|| ApiError::InvalidRequest("missing metadata".into()))?;
    let spec = manifest
        .spec
        .ok_or_else(|| ApiError::InvalidRequest("missing spec".into()))?;

    let mut annotations = Annotations::new();
    for (key, value) in metadata.annotations {
        annotations.insert(key, value);
    }
    let manifest = TaskManifest::new(metadata.name, convert_task_spec(spec)?)
        .and_then(|manifest| manifest.with_labels(convert_labels(metadata.labels)))
        .and_then(|manifest| manifest.with_annotations(annotations))
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    manifest
        .validate()
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    Ok(manifest)
}

/// Converts one domain task page into a protobuf list response.
pub(crate) fn tasks_page_to_proto(
    page: solti_model::TaskPage<solti_model::Task>,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    let remaining_item_count = u64::try_from(page.remaining_item_count).map_err(|_| {
        ApiError::Internal("remaining task count is outside the protobuf range".into())
    })?;
    let continuation = page
        .continuation
        .map(crate::continuation::encode)
        .transpose()?
        .unwrap_or_default();
    let tasks: Vec<proto_api::Task> = page
        .items
        .into_iter()
        .map(proto_api::Task::try_from)
        .collect::<Result<_, _>>()?;

    Ok(proto_api::ListTasksResponse {
        tasks,
        resource_version: page.resource_version,
        r#continue: continuation,
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Builds protobuf collection metadata for one Task prefix.
fn proto_metadata_for_prefix(
    page: &TaskPage<Task>,
    filter: &TaskFilter,
    keep: usize,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    let (continuation, remaining_item_count) =
        crate::continuation::prefix_metadata(page, filter, keep)?;
    let remaining_item_count = u64::try_from(remaining_item_count).map_err(|_| {
        ApiError::Internal("remaining task count is outside the protobuf range".into())
    })?;
    Ok(proto_api::ListTasksResponse {
        tasks: Vec::new(),
        resource_version: page.resource_version.clone(),
        r#continue: continuation
            .map(crate::continuation::encode)
            .transpose()?
            .unwrap_or_default(),
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Converts the largest complete prefix from one domain page within the protobuf limit.
pub(crate) fn tasks_page_to_proto_bounded(
    page: TaskPage<Task>,
    filter: &TaskFilter,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    tasks_page_to_proto_with_limit(page, filter, crate::MAX_TASK_LIST_RESPONSE_BYTES)
}

/// Converts one Task prefix within an explicit testable protobuf limit.
fn tasks_page_to_proto_with_limit(
    page: TaskPage<Task>,
    filter: &TaskFilter,
    limit: usize,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    if page
        .items
        .iter()
        .any(|task| !crate::visibility::task_is_visible(task))
    {
        return Err(ApiError::Internal(
            "handler returned an Embedded workload through the public gRPC API".into(),
        ));
    }

    let mut task_bytes = 0usize;
    let mut keep = page.items.is_empty().then_some(0);
    for (index, task) in page.items.iter().enumerate() {
        let proto = proto_api::Task::try_from(task.clone())?;
        task_bytes = task_bytes.saturating_add(prost::encoding::message::encoded_len(1, &proto));
        let candidate = index + 1;
        let metadata = proto_metadata_for_prefix(&page, filter, candidate)?;
        if metadata.encoded_len().saturating_add(task_bytes) <= limit {
            keep = Some(candidate);
        }
    }

    let keep = keep.ok_or_else(|| {
        ApiError::ResourceExhausted(format!(
            "the first Task exceeds the {limit}-byte list response limit"
        ))
    })?;
    let page = crate::continuation::retain_prefix(page, filter, keep)?;
    let response = tasks_page_to_proto(page)?;
    if response.encoded_len() > limit {
        return Err(ApiError::Internal(
            "bounded Task list exceeded its encoded response limit".into(),
        ));
    }
    Ok(response)
}

/// Converts one domain watch event into protobuf.
pub(crate) fn task_watch_event_to_proto(
    event: solti_model::TaskWatchEvent,
) -> Result<proto_api::WatchTasksResponse, ApiError> {
    let (event_type, task) = match event {
        solti_model::TaskWatchEvent::Added(task) => (proto_api::TaskWatchEventType::Added, task),
        solti_model::TaskWatchEvent::Modified(task) => {
            (proto_api::TaskWatchEventType::Modified, task)
        }
        solti_model::TaskWatchEvent::Deleted(task) => {
            (proto_api::TaskWatchEventType::Deleted, task)
        }
    };

    Ok(proto_api::WatchTasksResponse {
        r#type: event_type as i32,
        object: Some(proto_api::Task::try_from(task)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        EmbeddedSpec, ExtensionWorkload, Flag, SubprocessMode, SubprocessSpec, TaskContinuation,
        TaskEnv, TaskFilter, TaskId, TaskPhase, TaskSpec, TaskWorkload,
    };
    use std::time::UNIX_EPOCH;

    fn subprocess_workload() -> TaskWorkload {
        TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "ls".into(),
                args: vec![],
            },
            TaskEnv::new(),
            None,
            Flag::from(true),
        ))
    }

    #[test]
    fn task_converts_correctly() {
        let spec = TaskSpec::builder("my-slot", subprocess_workload(), 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("task-42", spec).unwrap();
        task.set_resource_version("1").unwrap();

        task.transition_starting(1, 1, "2").unwrap();
        task.transition_finished(1, 1, TaskPhase::Failed, Some("first".into()), None, "3")
            .unwrap();
        task.transition_starting(1, 2, "4").unwrap();
        task.transition_finished(1, 2, TaskPhase::Failed, Some("boom".into()), None, "5")
            .unwrap();
        task.transition_starting(1, 3, "6").unwrap();
        task.transition_finished(1, 3, TaskPhase::Failed, Some("boom".into()), None, "7")
            .unwrap();

        let created_ms = task
            .metadata()
            .creation_timestamp()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let bumped_version = task.metadata().resource_version().to_owned();

        let proto = proto_api::Task::try_from(task).expect("conversion must succeed");
        assert_eq!(proto.api_version, TASK_API_VERSION);
        assert_eq!(proto.kind, TASK_KIND);

        let meta = proto.metadata.unwrap();
        assert_eq!(meta.name, "task-42");
        assert!(!meta.uid.is_empty());
        assert_eq!(meta.creation_timestamp, created_ms);
        assert_eq!(meta.generation, 1);
        assert_eq!(meta.resource_version, bumped_version);

        let spec = proto.spec.unwrap();
        assert_eq!(spec.slot, "my-slot");

        let status = proto.status.unwrap();
        assert_eq!(status.phase, proto_api::TaskPhase::Failed as i32);
        assert_eq!(status.attempt, 3);
        assert_eq!(status.error, Some("boom".to_string()));
        assert_eq!(status.conditions.len(), 1);
        let condition = &status.conditions[0];
        assert_eq!(condition.r#type, "Reconciled");
        assert_eq!(condition.status, proto_api::ConditionStatus::True as i32);
        assert_eq!(condition.observed_generation, 1);
        assert_eq!(condition.reason, "RuntimeAccepted");
        assert!(!condition.message.is_empty());
        assert!(condition.last_transition_time > 0);
    }

    #[test]
    fn task_no_error() {
        let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("task-1", spec).unwrap();
        task.transition_starting(1, 1, "1").unwrap();
        task.transition_finished(1, 1, TaskPhase::Succeeded, None, Some(0), "2")
            .unwrap();

        let proto = proto_api::Task::try_from(task).expect("conversion must succeed");
        let status = proto.status.unwrap();
        assert_eq!(status.error, None);
        assert_eq!(status.exit_code, Some(0));
    }

    #[test]
    fn taskvisor_intake_pending_condition_converts_exactly() {
        let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("task-intake-pending", spec).unwrap();
        task.update_reconciliation_pending_diagnostic(
            "TaskvisorOwnershipAndControllerIntakePending",
            "waiting for Taskvisor ownership and controller intake capacity",
            "2",
        )
        .unwrap();

        let proto = proto_api::Task::try_from(task).unwrap();
        let condition = &proto.status.unwrap().conditions[0];
        assert_eq!(condition.status, proto_api::ConditionStatus::Unknown as i32);
        assert_eq!(condition.observed_generation, 1);
        assert_eq!(
            condition.reason,
            "TaskvisorOwnershipAndControllerIntakePending"
        );
        assert_eq!(
            condition.message,
            "waiting for Taskvisor ownership and controller intake capacity"
        );
    }

    #[test]
    fn list_response_carries_snapshot_continuation_and_remaining_count() {
        let mk = |id: &str| {
            let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
                .build()
                .unwrap();
            Task::new(id, spec).unwrap()
        };
        let continuation =
            TaskContinuation::new("store:9", TaskFilter::new(), TaskId::new("task-2").unwrap())
                .unwrap();
        let page = solti_model::TaskPage {
            items: vec![mk("task-1"), mk("task-2")],
            resource_version: "store:9".into(),
            continuation: Some(continuation.clone()),
            remaining_item_count: 3,
        };

        let resp = tasks_page_to_proto(page).expect("conversion must succeed");

        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.resource_version, "store:9");
        assert_eq!(resp.remaining_item_count, Some(3));
        assert_eq!(
            crate::continuation::decode(&resp.r#continue).unwrap(),
            continuation
        );
    }

    #[test]
    fn list_response_stops_at_the_exact_protobuf_boundary() {
        let mk = |id: &str| {
            let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
                .build()
                .unwrap();
            Task::new(id, spec).unwrap()
        };
        let page = TaskPage {
            items: vec![mk("task-1"), mk("task-2")],
            resource_version: "store:9".into(),
            continuation: None,
            remaining_item_count: 0,
        };
        let filter = TaskFilter::new();
        let first = proto_api::Task::try_from(page.items[0].clone()).unwrap();
        let first_limit = proto_metadata_for_prefix(&page, &filter, 1)
            .unwrap()
            .encoded_len()
            + prost::encoding::message::encoded_len(1, &first);

        let response = tasks_page_to_proto_with_limit(page.clone(), &filter, first_limit).unwrap();
        assert_eq!(response.encoded_len(), first_limit);
        assert_eq!(response.tasks.len(), 1);
        assert_eq!(response.remaining_item_count, Some(1));
        let continuation = crate::continuation::decode(&response.r#continue).unwrap();
        assert_eq!(continuation.after().as_str(), "task-1");

        assert!(matches!(
            tasks_page_to_proto_with_limit(page, &filter, first_limit - 1),
            Err(ApiError::ResourceExhausted(_))
        ));
    }

    #[test]
    fn max_manifest_task_can_fit_one_native_protobuf_page() {
        let manifest = |padding: usize| {
            let workload = TaskWorkload::Extension(
                ExtensionWorkload::new(
                    "workloads.example.io/v1",
                    "LargePayload",
                    serde_json::json!({ "padding": "x".repeat(padding) }),
                )
                .unwrap(),
            );
            let spec = TaskSpec::builder("large", workload, 5_000_u64)
                .build()
                .unwrap();
            TaskManifest::new("large", spec).unwrap()
        };
        let empty = manifest(0);
        let padding = crate::MAX_TASK_LIST_RESPONSE_BYTES
            .checked_sub(serde_json::to_vec(&empty).unwrap().len())
            .unwrap();
        let manifest = manifest(padding);
        assert_eq!(
            serde_json::to_vec(&manifest).unwrap().len(),
            crate::MAX_TASK_LIST_RESPONSE_BYTES
        );

        let resource_version = "AAAAAAAAAAAAAAAAAAAAAA:1";
        let mut task = Task::from_manifest(manifest).unwrap();
        task.set_resource_version(resource_version).unwrap();
        assert!(serde_json::to_vec(&task).unwrap().len() > crate::MAX_TASK_LIST_RESPONSE_BYTES);
        let response = tasks_page_to_proto_bounded(
            TaskPage {
                items: vec![task],
                resource_version: resource_version.into(),
                continuation: None,
                remaining_item_count: 0,
            },
            &TaskFilter::new(),
        )
        .unwrap();

        assert_eq!(response.tasks.len(), 1);
        assert!(response.encoded_len() <= crate::MAX_TASK_LIST_RESPONSE_BYTES);
    }

    #[test]
    fn handler_output_with_embedded_workload_is_an_internal_error() {
        let spec = TaskSpec::builder(
            "slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .unwrap();
        let task = Task::new("task-1", spec).unwrap();
        let err = proto_api::Task::try_from(task).unwrap_err();
        assert!(matches!(&err, ApiError::Internal(msg) if msg.contains("Embedded")));
    }

    #[test]
    fn request_rejects_wrong_task_gvk() {
        let proto = proto_api::TaskManifest {
            api_version: "other.io/v1".into(),
            kind: TASK_KIND.into(),
            metadata: Some(proto_api::TaskManifestMeta {
                name: "task-1".into(),
                ..Default::default()
            }),
            spec: None,
        };

        let err = task_manifest_from_proto(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("apiVersion")));
    }

    #[test]
    fn request_rejects_invalid_user_metadata() {
        let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
            .build()
            .unwrap();
        let mut labels = std::collections::HashMap::new();
        labels.insert("bad key".to_owned(), "value".to_owned());
        let proto = proto_api::TaskManifest {
            api_version: TASK_API_VERSION.into(),
            kind: TASK_KIND.into(),
            metadata: Some(proto_api::TaskManifestMeta {
                name: "task-1".into(),
                labels,
                ..Default::default()
            }),
            spec: Some(spec_to_proto(&spec).unwrap()),
        };

        let err = task_manifest_from_proto(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("label key")));
    }

    #[test]
    fn request_manifest_converts_only_desired_state() {
        let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
            .build()
            .unwrap();
        let proto = proto_api::TaskManifest {
            api_version: TASK_API_VERSION.into(),
            kind: TASK_KIND.into(),
            metadata: Some(proto_api::TaskManifestMeta {
                name: "task-1".into(),
                labels: [("app.kubernetes.io/name".into(), "worker".into())]
                    .into_iter()
                    .collect(),
                annotations: [("example.io/note".into(), "desired".into())]
                    .into_iter()
                    .collect(),
            }),
            spec: Some(spec_to_proto(&spec).unwrap()),
        };

        let manifest = task_manifest_from_proto(proto).unwrap();

        assert_eq!(manifest.name(), "task-1");
        assert_eq!(
            manifest.metadata().labels().get("app.kubernetes.io/name"),
            Some("worker")
        );
        assert_eq!(
            manifest.metadata().annotations().get("example.io/note"),
            Some("desired")
        );
    }
}
