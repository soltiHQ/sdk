//! # Task run record.
//!
//! [`TaskRun`] captures a single execution attempt with start/finish times and outcome.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::TaskPhase;

/// Record of a single task execution attempt.
///
/// Each time the supervisor starts a task, a new `TaskRun` is created.
/// When the attempt finishes (success, failure, timeout, etc.), the run
/// is closed with the terminal phase and timestamp.
///
/// Runs are associated with a [`Task`](crate::Task) via its [`TaskId`](crate::TaskId) and ordered by attempt number.
///
/// ## Also
///
/// - [`Task`](crate::Task) parent resource.
/// - [`TaskPhase`] phase values stored in `phase` field.
///
/// # Lifecycle
///
/// ```text
///   TaskStarting  ──►  TaskRun { phase: Running, finished_at: None }
///        │
///        ├──► TaskStopped   ──►  phase = Succeeded, finished_at = Some(now)
///        ├──► TaskFailed    ──►  phase = Failed,    finished_at = Some(now)
///        ├──► TimeoutHit    ──►  phase = Timeout,   finished_at = Some(now)
///        └──► ActorExhausted ──► phase = Exhausted, finished_at = Some(now)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRun {
    /// Attempt number (1-based, matches the task's attempt counter after increment).
    pub attempt: u32,
    /// Phase this run ended in (or `Running` if still active).
    pub phase: TaskPhase,
    /// When the run started.
    #[serde(with = "super::metadata::time_serde")]
    pub started_at: SystemTime,
    /// When the run finished (`None` while still running).
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "option_time_serde",
        default
    )]
    pub finished_at: Option<SystemTime>,
    /// Error message (present when phase is Failed/Timeout/Exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Process exit code (Subprocess/Container only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl TaskRun {
    /// Create a new run record for an attempt that just started.
    pub fn starting(attempt: u32) -> Self {
        Self {
            attempt,
            phase: TaskPhase::Running,
            started_at: SystemTime::now(),
            finished_at: None,
            error: None,
            exit_code: None,
        }
    }

    /// Close the run with a terminal phase.
    pub fn finish(&mut self, phase: TaskPhase, error: Option<String>, exit_code: Option<i32>) {
        self.finished_at = Some(SystemTime::now());
        self.phase = phase;
        self.error = error;
        self.exit_code = exit_code;
    }

    /// Whether this run is still in progress.
    pub fn is_active(&self) -> bool {
        self.finished_at.is_none()
    }
}

mod option_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match time {
            Some(t) => {
                let since_epoch = t
                    .duration_since(UNIX_EPOCH)
                    .map_err(serde::ser::Error::custom)?;
                let ms = since_epoch.as_secs() * 1_000 + u64::from(since_epoch.subsec_millis());
                ms.serialize(serializer)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<u64> = Option::deserialize(deserializer)?;
        Ok(opt.map(|ms| UNIX_EPOCH + std::time::Duration::from_millis(ms)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_creates_running_run() {
        let run = TaskRun::starting(1);
        assert_eq!(run.attempt, 1);
        assert_eq!(run.phase, TaskPhase::Running);
        assert!(run.is_active());
        assert!(run.finished_at.is_none());
        assert!(run.error.is_none());
        assert!(run.exit_code.is_none());
    }

    #[test]
    fn finish_closes_run() {
        let mut run = TaskRun::starting(2);
        run.finish(TaskPhase::Failed, Some("boom".into()), Some(1));

        assert!(!run.is_active());
        assert!(run.finished_at.is_some());
        assert_eq!(run.phase, TaskPhase::Failed);
        assert_eq!(run.error.as_deref(), Some("boom"));
        assert_eq!(run.exit_code, Some(1));
    }

    #[test]
    fn finish_succeeded_no_error() {
        let mut run = TaskRun::starting(1);
        run.finish(TaskPhase::Succeeded, None, None);

        assert!(!run.is_active());
        assert_eq!(run.phase, TaskPhase::Succeeded);
        assert!(run.error.is_none());
        assert!(run.exit_code.is_none());
    }

    #[test]
    fn serde_roundtrip_active() {
        let run = TaskRun::starting(3);
        let json = serde_json::to_string(&run).unwrap();
        let back: TaskRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.attempt, 3);
        assert_eq!(back.phase, TaskPhase::Running);
        assert!(back.finished_at.is_none());
    }

    #[test]
    fn serde_roundtrip_finished() {
        let mut run = TaskRun::starting(1);
        run.finish(TaskPhase::Timeout, Some("timeout".into()), None);

        let json = serde_json::to_string(&run).unwrap();
        let back: TaskRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.phase, TaskPhase::Timeout);
        assert!(back.finished_at.is_some());
        assert_eq!(back.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn serde_skips_none_fields() {
        let run = TaskRun::starting(1);
        let json = serde_json::to_string(&run).unwrap();
        assert!(!json.contains("finishedAt"));
        assert!(!json.contains("error"));
        assert!(!json.contains("exitCode"));
    }
}
