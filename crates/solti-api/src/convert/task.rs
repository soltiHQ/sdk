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
            status: Some(proto_api::TaskStatusInfo {
                phase: proto_api::TaskStatus::from(status.phase) as i32,
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
    let tasks: Vec<proto_api::TaskData> = page
        .items
        .into_iter()
        .filter(|t| !matches!(t.spec().kind(), TaskKind::Embedded))
        .map(proto_api::TaskData::try_from)
        .collect::<Result<_, _>>()?;

    Ok(proto_api::ListTasksResponse {
        total: tasks.len() as u32,
        tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskPhase, TaskSpec,
    };
    use std::time::UNIX_EPOCH;

    fn subprocess_task_kind() -> TaskKind {
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "ls".into(),
                args: vec![],
            },
            env: TaskEnv::new(),
            cwd: None,
            fail_on_non_zero: Flag::from(true),
        })
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
        assert_eq!(status.phase, proto_api::TaskStatus::Failed as i32);
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
