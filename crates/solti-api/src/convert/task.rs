//! # `Task` / `TaskPage` domain to wire conversion.

use solti_model::{Task, TaskKind};

use super::spec::spec_to_proto;
use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<Task> for proto_api::TaskData {
    type Error = ApiError;

    fn try_from(task: Task) -> Result<Self, Self::Error> {
        let (metadata, spec, status) = task.into_parts();

        Ok(proto_api::TaskData {
            metadata: Some(proto_api::ObjectMeta::from(&metadata)),
            spec: Some(spec_to_proto(&spec)?),
            status: Some(proto_api::TaskStatus {
                phase: proto_api::TaskPhase::from(status.phase) as i32,
                exit_code: status.exit_code,
                attempt: status.attempt,
                error: status.error,
            }),
        })
    }
}

/// Build a proto `ListTasksResponse` from a domain `TaskPage`.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn tasks_page_to_proto(
    page: solti_model::TaskPage<solti_model::Task>,
) -> Result<proto_api::ListTasksResponse, ApiError> {
    // `total` is the count of all matching tasks across pages, not the size of this
    // page — preserve it from the domain page (saturating into the proto's u32).
    let total = u32::try_from(page.total).unwrap_or(u32::MAX);
    let tasks: Vec<proto_api::TaskData> = page
        .items
        .into_iter()
        .filter(|t| !matches!(t.spec().kind(), TaskKind::Embedded))
        .map(proto_api::TaskData::try_from)
        .collect::<Result<_, _>>()?;

    Ok(proto_api::ListTasksResponse { total, tasks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskPhase, TaskSpec,
    };
    use std::time::UNIX_EPOCH;

    fn subprocess_task_kind() -> TaskKind {
        TaskKind::Subprocess(SubprocessSpec::new(
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
        let spec = TaskSpec::builder("my-slot", subprocess_task_kind(), 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("task-42".into(), spec);

        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("first".into()), None)
            .unwrap();
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), None)
            .unwrap();
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), None)
            .unwrap();

        let created_ms = task
            .metadata()
            .created_at
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let bumped_version = task.metadata().resource_version;
        assert!(
            bumped_version > 1,
            "resource_version should bump on transitions",
        );

        let proto = proto_api::TaskData::try_from(task).expect("conversion must succeed");

        let meta = proto.metadata.unwrap();
        assert_eq!(meta.id, "task-42");
        assert_eq!(meta.created_at, created_ms);
        assert_eq!(meta.resource_version, bumped_version);

        let spec = proto.spec.unwrap();
        assert_eq!(spec.slot, "my-slot");

        let status = proto.status.unwrap();
        assert_eq!(status.phase, proto_api::TaskPhase::Failed as i32);
        assert_eq!(status.attempt, 3);
        assert_eq!(status.error, Some("boom".to_string()));
    }

    #[test]
    fn task_no_error() {
        let spec = TaskSpec::builder("slot", subprocess_task_kind(), 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("task-1".into(), spec);
        task.transition_starting();
        task.transition_finished(TaskPhase::Succeeded, None, Some(0))
            .unwrap();

        let proto = proto_api::TaskData::try_from(task).expect("conversion must succeed");
        let status = proto.status.unwrap();
        assert_eq!(status.error, None);
        assert_eq!(status.exit_code, Some(0));
    }

    #[test]
    fn list_response_total_reflects_full_match_count_not_page_size() {
        // Simulate a paginated query: 5 total matches, but this page carries only 2 items.
        let mk = |id: &str| {
            let spec = TaskSpec::builder("slot", subprocess_task_kind(), 5_000_u64)
                .build()
                .unwrap();
            Task::new(id.into(), spec)
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
    fn task_embedded_rejected() {
        let spec = TaskSpec::builder("slot", TaskKind::Embedded, 5_000_u64)
            .build()
            .unwrap();
        let task = Task::new("task-1".into(), spec);
        let err = proto_api::TaskData::try_from(task).unwrap_err();
        assert!(
            matches!(&err, ApiError::InvalidRequest(msg) if msg.contains("embedded tasks")),
            "expected InvalidRequest for Embedded task, got {err:?}",
        );
    }
}
