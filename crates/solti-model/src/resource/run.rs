//! # Task run
//!
//! [`TaskRun`] records one execution attempt.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{ModelError, ModelResult, TaskPhase, WorkloadTypeMeta};

/// Record of one execution attempt.
///
/// An active run has phase `Running` and no terminal fields.
/// A terminal run has a terminal phase and `finishedAt`.
/// Terminal `error` and `exitCode` values are optional details and do not
/// determine the phase.
/// The finish timestamp records the supervisor's logical outcome. It does not
/// prove physical exit after a force-abort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "raw::TaskRunRaw")]
pub struct TaskRun {
    workload: WorkloadTypeMeta,
    generation: u64,
    attempt: u32,
    phase: TaskPhase,
    #[serde(with = "super::metadata::rfc3339_time_serde")]
    started_at: SystemTime,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "super::metadata::rfc3339_time_serde::option",
        default
    )]
    finished_at: Option<SystemTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for TaskRun {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TaskRun".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        crate::schema::task_run(generator)
    }
}

impl TaskRun {
    /// Creates an active run.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when generation or attempt is zero.
    pub fn starting(
        generation: u64,
        attempt: u32,
        workload: WorkloadTypeMeta,
    ) -> ModelResult<Self> {
        Self::from_parts(
            workload,
            generation,
            attempt,
            TaskPhase::Running,
            super::metadata::time_serde::now(),
            None,
            None,
            None,
        )
    }

    /// Reconstructs a run from serialized fields.
    ///
    /// Diagnostics longer than [`MAX_TASK_DIAGNOSTIC_BYTES`](crate::MAX_TASK_DIAGNOSTIC_BYTES)
    /// are truncated to a UTF-8-safe prefix.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when identity fields, phase, timestamp
    /// presence, or terminal diagnostics are inconsistent.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        workload: WorkloadTypeMeta,
        generation: u64,
        attempt: u32,
        phase: TaskPhase,
        started_at: SystemTime,
        finished_at: Option<SystemTime>,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> ModelResult<Self> {
        let run = Self {
            workload,
            generation,
            attempt,
            phase,
            started_at,
            finished_at,
            error: error.map(super::status::truncate_task_diagnostic),
            exit_code,
        };
        run.validate()?;
        Ok(run)
    }

    /// Finishes an active run.
    ///
    /// Diagnostics longer than [`MAX_TASK_DIAGNOSTIC_BYTES`](crate::MAX_TASK_DIAGNOSTIC_BYTES)
    /// are truncated to a UTF-8-safe prefix.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the run is already finished or `phase` is not terminal.
    pub fn finish(
        &mut self,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> ModelResult<()> {
        if !self.is_active() {
            return Err(ModelError::Invalid("task run is already finished".into()));
        }
        if !phase.is_terminal() {
            return Err(ModelError::Invalid(
                format!("task run requires a terminal phase, got {phase}").into(),
            ));
        }
        self.finished_at = Some(super::metadata::time_serde::now());
        self.phase = phase;
        self.error = error.map(super::status::truncate_task_diagnostic);
        self.exit_code = exit_code;
        Ok(())
    }

    /// Workload GVK executed by this run.
    pub fn workload(&self) -> &WorkloadTypeMeta {
        &self.workload
    }

    /// Desired-state generation executed by this run.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// One-based attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Current or terminal run phase.
    pub fn phase(&self) -> TaskPhase {
        self.phase
    }

    /// Recorded logical start timestamp.
    ///
    /// The model does not establish the timestamp's provenance or compare it
    /// with `finishedAt`.
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    /// Time when the supervisor recorded the terminal run outcome.
    pub fn finished_at(&self) -> Option<SystemTime> {
        self.finished_at
    }

    /// Terminal diagnostic, when available.
    ///
    /// This detail does not determine the terminal phase.
    /// The value is at most [`MAX_TASK_DIAGNOSTIC_BYTES`](crate::MAX_TASK_DIAGNOSTIC_BYTES)
    /// UTF-8 bytes.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Process exit code, when available.
    ///
    /// This detail does not determine the terminal phase.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Returns the serialized run fields.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        WorkloadTypeMeta,
        u64,
        u32,
        TaskPhase,
        SystemTime,
        Option<SystemTime>,
        Option<String>,
        Option<i32>,
    ) {
        (
            self.workload,
            self.generation,
            self.attempt,
            self.phase,
            self.started_at,
            self.finished_at,
            self.error,
            self.exit_code,
        )
    }

    /// Returns whether the run is active.
    pub fn is_active(&self) -> bool {
        self.phase == TaskPhase::Running
    }

    fn validate(&self) -> ModelResult<()> {
        if self.generation == 0 {
            return Err(ModelError::Invalid(
                "task run generation must be greater than zero".into(),
            ));
        }
        if self.attempt == 0 {
            return Err(ModelError::Invalid(
                "task run attempt must be greater than zero".into(),
            ));
        }
        match self.phase {
            TaskPhase::Running if self.finished_at.is_none() => {
                if self.error.is_some() || self.exit_code.is_some() {
                    return Err(ModelError::Invalid(
                        "active task run cannot contain terminal diagnostics".into(),
                    ));
                }
            }
            phase if phase.is_terminal() && self.finished_at.is_some() => {}
            TaskPhase::Running => {
                return Err(ModelError::Invalid(
                    "active task run cannot have finishedAt".into(),
                ));
            }
            phase if phase.is_terminal() => {
                return Err(ModelError::Invalid(
                    "terminal task run requires finishedAt".into(),
                ));
            }
            _ => {
                return Err(ModelError::Invalid(
                    "task run phase must be Running or terminal".into(),
                ));
            }
        }
        Ok(())
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskRunRaw {
        workload: WorkloadTypeMeta,
        generation: u64,
        attempt: u32,
        phase: TaskPhase,
        #[serde(with = "super::super::metadata::rfc3339_time_serde")]
        started_at: SystemTime,
        #[serde(default, with = "super::super::metadata::rfc3339_time_serde::option")]
        finished_at: Option<SystemTime>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        exit_code: Option<i32>,
    }

    impl TryFrom<TaskRunRaw> for TaskRun {
        type Error = ModelError;

        fn try_from(raw: TaskRunRaw) -> Result<Self, Self::Error> {
            TaskRun::from_parts(
                raw.workload,
                raw.generation,
                raw.attempt,
                raw.phase,
                raw.started_at,
                raw.finished_at,
                raw.error,
                raw.exit_code,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload() -> WorkloadTypeMeta {
        WorkloadTypeMeta::new("example.io/v1", "Example").unwrap()
    }

    #[test]
    fn finish_requires_terminal_phase_and_active_run() {
        let mut run = TaskRun::starting(1, 1, workload()).unwrap();
        assert!(run.finish(TaskPhase::Running, None, None).is_err());
        run.finish(TaskPhase::Succeeded, None, Some(0)).unwrap();
        assert!(run.finish(TaskPhase::Failed, None, Some(1)).is_err());
    }

    #[test]
    fn serde_rejects_inconsistent_and_unknown_run_fields() {
        let run = TaskRun::starting(1, 1, workload()).unwrap();
        let mut json = serde_json::to_value(run).unwrap();
        json["attempt"] = serde_json::json!(0);
        assert!(serde_json::from_value::<TaskRun>(json).is_err());

        let run = TaskRun::starting(1, 1, workload()).unwrap();
        let mut json = serde_json::to_value(run).unwrap();
        json["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskRun>(json).is_err());

        let run = TaskRun::starting(1, 1, workload()).unwrap();
        let mut finished_active = serde_json::to_value(&run).unwrap();
        finished_active["finishedAt"] = serde_json::json!("2026-01-01T00:00:00Z");
        assert!(serde_json::from_value::<TaskRun>(finished_active).is_err());

        let mut unfinished_terminal = serde_json::to_value(run).unwrap();
        unfinished_terminal["phase"] = serde_json::json!("succeeded");
        assert!(serde_json::from_value::<TaskRun>(unfinished_terminal).is_err());
    }

    #[test]
    fn run_diagnostic_is_bounded_across_finish_construction_and_deserialization() {
        let exact = "a".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES);
        let mut run = TaskRun::starting(1, 1, workload()).unwrap();
        run.finish(TaskPhase::Failed, Some(exact.clone()), None)
            .unwrap();
        assert_eq!(run.error(), Some(exact.as_str()));

        let ascii_over = "b".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES + 1);
        let reconstructed = TaskRun::from_parts(
            workload(),
            1,
            1,
            TaskPhase::Failed,
            SystemTime::UNIX_EPOCH,
            Some(SystemTime::UNIX_EPOCH),
            Some(ascii_over.clone()),
            None,
        )
        .unwrap();
        assert_eq!(
            reconstructed.error(),
            Some(&ascii_over[..crate::MAX_TASK_DIAGNOSTIC_BYTES])
        );

        let multibyte_over = format!("{}é", "c".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES - 1));
        let mut serialized = serde_json::to_value(run).unwrap();
        serialized["error"] = serde_json::json!(multibyte_over);
        let deserialized: TaskRun = serde_json::from_value(serialized).unwrap();
        let error = deserialized.error().unwrap();
        assert_eq!(error, "c".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES - 1));
        assert!(error.is_char_boundary(error.len()));
        assert!(error.len() <= crate::MAX_TASK_DIAGNOSTIC_BYTES);
    }
}
