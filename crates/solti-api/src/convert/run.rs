//! # `TaskRun` to `TaskRunInfo` conversion.

use solti_model::TaskRun;

use super::time::system_time_to_ms;
use crate::proto_api;

impl From<TaskRun> for proto_api::TaskRunInfo {
    fn from(run: TaskRun) -> Self {
        proto_api::TaskRunInfo {
            phase: proto_api::TaskPhase::from(run.phase) as i32,
            finished_at: run.finished_at.map(system_time_to_ms),
            started_at: system_time_to_ms(run.started_at),
            exit_code: run.exit_code,
            attempt: run.attempt,
            error: run.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::TaskPhase;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn run_converts_all_fields() {
        let started = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let finished = UNIX_EPOCH + Duration::from_millis(1_700_000_001_500);

        let mut run = TaskRun::starting(2);
        run.started_at = started;
        run.finished_at = Some(finished);
        run.phase = TaskPhase::Failed;
        run.error = Some("boom".into());
        run.exit_code = Some(137);

        let proto = proto_api::TaskRunInfo::from(run);

        assert_eq!(proto.attempt, 2);
        assert_eq!(proto.phase, proto_api::TaskPhase::Failed as i32);
        assert_eq!(proto.started_at, 1_700_000_000_000);
        assert_eq!(proto.finished_at, Some(1_700_000_001_500));
        assert_eq!(proto.error.as_deref(), Some("boom"));
        assert_eq!(proto.exit_code, Some(137));
    }

    #[test]
    fn run_active_has_no_finished_timestamp() {
        let run = TaskRun::starting(1);
        let proto = proto_api::TaskRunInfo::from(run);
        assert_eq!(proto.finished_at, None);
        assert_eq!(proto.exit_code, None);
    }
}
