//! # Task resource conversion.

use solti_model::{Annotations, TASK_API_VERSION, TASK_KIND, Task, TaskManifest};

use super::spec::{convert_labels, convert_task_spec, spec_to_proto};
use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<Task> for proto_api::Task {
    type Error = ApiError;

    fn try_from(task: Task) -> Result<Self, Self::Error> {
        let (type_meta, metadata, spec, status) = task.into_parts();

        Ok(proto_api::Task {
            api_version: type_meta.api_version().to_owned(),
            kind: type_meta.kind().to_owned(),
            metadata: Some(proto_api::ObjectMeta::from(&metadata)),
            spec: Some(spec_to_proto(&spec)?),
            status: Some(proto_api::TaskStatus {
                observed_generation: status.observed_generation,
                phase: proto_api::TaskPhase::from(status.phase) as i32,
                exit_code: status.exit_code,
                attempt: status.attempt,
                error: status.error,
                conditions: status.conditions.into_iter().map(Into::into).collect(),
            }),
        })
    }
}

/// Convert a create/apply wire manifest into the domain desired state.
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
        .map(|manifest| {
            manifest
                .with_labels(convert_labels(metadata.labels))
                .with_annotations(annotations)
        })
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    manifest
        .validate()
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    Ok(manifest)
}

/// Build a proto `ListTasksResponse` from a domain `TaskPage`.
pub(crate) fn tasks_page_to_proto(
    page: solti_model::TaskPage<solti_model::Task>,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    // `total` is the count of all matching tasks across pages, not the size of this
    // page — preserve it from the domain page (saturating into the proto's u32).
    let total = u32::try_from(page.total).unwrap_or(u32::MAX);
    let tasks: Vec<proto_api::Task> = page
        .items
        .into_iter()
        .map(proto_api::Task::try_from)
        .collect::<Result<_, _>>()?;

    Ok(proto_api::ListTasksResponse { total, tasks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        EmbeddedSpec, Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskPhase, TaskSpec,
        TaskWorkload,
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
    fn list_response_total_reflects_full_match_count_not_page_size() {
        // Simulate a paginated query: 5 total matches, but this page carries only 2 items.
        let mk = |id: &str| {
            let spec = TaskSpec::builder("slot", subprocess_workload(), 5_000_u64)
                .build()
                .unwrap();
            Task::new(id, spec).unwrap()
        };
        let page = solti_model::TaskPage {
            items: vec![mk("task-1"), mk("task-2")],
            total: 5,
        };

        let resp = tasks_page_to_proto(page).expect("conversion must succeed");

        assert_eq!(resp.tasks.len(), 2, "this page carries exactly 2 items");
        assert_eq!(
            resp.total, 5,
            "total must report all matching tasks across pages, not the current page size"
        );
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
