//! Task run record.
//!
//! [`TaskRun`] captures one execution attempt.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{TaskPhase, WorkloadTypeMeta};

/// Record of a single task execution attempt.
///
/// Each time the supervisor starts a task, a new `TaskRun` is created.
/// When the attempt finishes, the run is closed with a terminal phase and timestamp.
///
/// Runs are identified by `(generation, attempt)`.
///
/// ## Example
///
/// ```
/// use solti_model::{TaskPhase, TaskRun, WorkloadTypeMeta};
///
/// let workload = WorkloadTypeMeta::new("example.io/v1", "Example").unwrap();
/// let mut run = TaskRun::starting(1, 1, workload);
/// assert!(run.is_active());
///
/// run.finish(TaskPhase::Succeeded, None, Some(0));
/// assert!(!run.is_active());
/// assert_eq!(run.phase, TaskPhase::Succeeded);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRun {
    /// Workload GVK executed by this historical generation.
    pub workload: WorkloadTypeMeta,
    /// Desired-state generation executed by this run.
    pub generation: u64,
    /// Attempt number (1-based, matches the task's attempt counter after increment).
    pub attempt: u32,
    /// Phase this run ended in (or `Running` if still active).
    pub phase: TaskPhase,
    /// When the run started.
    #[serde(with = "super::metadata::rfc3339_time_serde")]
    pub started_at: SystemTime,
    /// When the run finished (`None` while still running).
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "super::metadata::rfc3339_time_serde::option",
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
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{TaskPhase, TaskRun, WorkloadTypeMeta};
    ///
    /// let workload = WorkloadTypeMeta::new("example.io/v1", "Example").unwrap();
    /// let run = TaskRun::starting(1, 2, workload);
    /// assert_eq!(run.attempt, 2);
    /// assert_eq!(run.phase, TaskPhase::Running);
    /// assert!(run.finished_at.is_none());
    /// ```
    pub fn starting(generation: u64, attempt: u32, workload: WorkloadTypeMeta) -> Self {
        Self {
            workload,
            generation,
            attempt,
            phase: TaskPhase::Running,
            started_at: super::metadata::time_serde::now(),
            finished_at: None,
            error: None,
            exit_code: None,
        }
    }

    /// Close the run with a terminal phase.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{TaskPhase, TaskRun, WorkloadTypeMeta};
    ///
    /// let workload = WorkloadTypeMeta::new("example.io/v1", "Example").unwrap();
    /// let mut run = TaskRun::starting(1, 1, workload);
    /// run.finish(TaskPhase::Failed, Some("boom".into()), Some(1));
    ///
    /// assert_eq!(run.error.as_deref(), Some("boom"));
    /// assert_eq!(run.exit_code, Some(1));
    /// ```
    pub fn finish(&mut self, phase: TaskPhase, error: Option<String>, exit_code: Option<i32>) {
        self.finished_at = Some(super::metadata::time_serde::now());
        self.phase = phase;
        self.error = error;
        self.exit_code = exit_code;
    }

    /// Return whether this run is still in progress.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{TaskPhase, TaskRun, WorkloadTypeMeta};
    ///
    /// let workload = WorkloadTypeMeta::new("example.io/v1", "Example").unwrap();
    /// let mut run = TaskRun::starting(1, 1, workload);
    /// assert!(run.is_active());
    ///
    /// run.finish(TaskPhase::Succeeded, None, None);
    /// assert!(!run.is_active());
    /// ```
    pub fn is_active(&self) -> bool {
        self.finished_at.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn workload() -> WorkloadTypeMeta {
        WorkloadTypeMeta::new("example.io/v1", "Example").unwrap()
    }

    #[test]
    fn starting_creates_running_run() {
        let run = TaskRun::starting(1, 1, workload());
        assert_eq!(run.attempt, 1);
        assert_eq!(run.phase, TaskPhase::Running);
        assert!(run.is_active());
        assert!(run.finished_at.is_none());
        assert!(run.error.is_none());
        assert!(run.exit_code.is_none());
    }

    #[test]
    fn finish_closes_run() {
        let mut run = TaskRun::starting(1, 2, workload());
        run.finish(TaskPhase::Failed, Some("boom".into()), Some(1));

        assert!(!run.is_active());
        assert!(run.finished_at.is_some());
        assert_eq!(run.phase, TaskPhase::Failed);
        assert_eq!(run.error.as_deref(), Some("boom"));
        assert_eq!(run.exit_code, Some(1));
    }

    #[test]
    fn finish_succeeded_no_error() {
        let mut run = TaskRun::starting(1, 1, workload());
        run.finish(TaskPhase::Succeeded, None, None);

        assert!(!run.is_active());
        assert_eq!(run.phase, TaskPhase::Succeeded);
        assert!(run.error.is_none());
        assert!(run.exit_code.is_none());
    }

    #[test]
    fn serde_roundtrip_active() {
        let run = TaskRun::starting(4, 3, workload());
        let json = serde_json::to_string(&run).unwrap();
        let back: TaskRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.attempt, 3);
        assert_eq!(back.generation, 4);
        assert_eq!(back.phase, TaskPhase::Running);
        assert!(back.finished_at.is_none());
    }

    #[test]
    fn serde_roundtrip_finished() {
        let mut run = TaskRun::starting(2, 1, workload());
        run.finish(TaskPhase::Timeout, Some("timeout".into()), None);

        let json = serde_json::to_string(&run).unwrap();
        let back: TaskRun = serde_json::from_str(&json).unwrap();

        assert_eq!(back.phase, TaskPhase::Timeout);
        assert!(back.finished_at.is_some());
        assert_eq!(back.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn serde_uses_rfc3339_resource_timestamps() {
        let mut run = TaskRun::starting(1, 1, workload());
        run.started_at = UNIX_EPOCH + Duration::from_millis(1_712_750_400_123);
        run.finished_at = Some(UNIX_EPOCH + Duration::from_millis(1_712_750_402_456));

        let json = serde_json::to_value(run).unwrap();

        assert_eq!(json["startedAt"], "2024-04-10T12:00:00.123Z");
        assert_eq!(json["finishedAt"], "2024-04-10T12:00:02.456Z");
    }

    #[test]
    fn serde_rejects_unix_milliseconds_for_resource_timestamps() {
        let run = TaskRun::starting(1, 1, workload());
        let mut json = serde_json::to_value(run).unwrap();
        json["startedAt"] = serde_json::json!(1_712_750_400_000_u64);

        assert!(serde_json::from_value::<TaskRun>(json).is_err());
    }

    #[test]
    fn serde_skips_none_fields() {
        let run = TaskRun::starting(1, 1, workload());
        let json = serde_json::to_string(&run).unwrap();
        assert!(!json.contains("finishedAt"));
        assert!(!json.contains("error"));
        assert!(!json.contains("exitCode"));
    }
}
