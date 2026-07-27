//! # `TaskRun` to `TaskRunInfo` conversion.

use solti_model::TaskRun;

use super::time::system_time_to_ms;
use crate::error::ApiError;
use crate::proto_api;
use crate::visibility::run_is_visible;

impl TryFrom<TaskRun> for proto_api::TaskRunInfo {
    type Error = ApiError;

    fn try_from(run: TaskRun) -> Result<Self, Self::Error> {
        if !run_is_visible(&run) {
            return Err(ApiError::Internal(
                "handler returned an Embedded task run with no wire representation".into(),
            ));
        }

        let (workload, generation, attempt, phase, started_at, finished_at, error, exit_code) =
            run.into_parts();
        Ok(proto_api::TaskRunInfo {
            workload_api_version: workload.api_version().to_owned(),
            workload_kind: workload.kind().to_owned(),
            generation,
            phase: proto_api::TaskPhase::try_from(phase)? as i32,
            finished_at: finished_at.map(system_time_to_ms).transpose()?,
            started_at: system_time_to_ms(started_at)?,
            exit_code,
            attempt,
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{TaskPhase, WORKLOAD_API_VERSION, WorkloadTypeMeta};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn run_converts_all_fields() {
        let started = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let finished = UNIX_EPOCH + Duration::from_millis(1_700_000_001_500);

        let workload = WorkloadTypeMeta::new("example.io/v1", "DatabaseBackup").unwrap();
        let run = TaskRun::from_parts(
            workload,
            3,
            2,
            TaskPhase::Failed,
            started,
            Some(finished),
            Some("boom".into()),
            Some(137),
        )
        .unwrap();

        let proto = proto_api::TaskRunInfo::try_from(run).unwrap();

        assert_eq!(proto.workload_api_version, "example.io/v1");
        assert_eq!(proto.workload_kind, "DatabaseBackup");
        assert_eq!(proto.attempt, 2);
        assert_eq!(proto.generation, 3);
        assert_eq!(proto.phase, proto_api::TaskPhase::Failed as i32);
        assert_eq!(proto.started_at, 1_700_000_000_000);
        assert_eq!(proto.finished_at, Some(1_700_000_001_500));
        assert_eq!(proto.error.as_deref(), Some("boom"));
        assert_eq!(proto.exit_code, Some(137));
    }

    #[test]
    fn run_active_has_no_finished_timestamp() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let run = TaskRun::starting(1, 1, workload).unwrap();
        let proto = proto_api::TaskRunInfo::try_from(run).unwrap();
        assert_eq!(proto.finished_at, None);
        assert_eq!(proto.exit_code, None);
    }

    #[test]
    fn embedded_run_is_not_exposed() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Embedded").unwrap();
        let run = TaskRun::starting(1, 1, workload).unwrap();

        let error = proto_api::TaskRunInfo::try_from(run).unwrap_err();
        assert!(matches!(error, ApiError::Internal(message) if message.contains("Embedded")));
    }
}
