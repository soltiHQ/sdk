//! # Task resource.
//!
//! [`Task`] is the K8s-style aggregate: metadata + spec + status.

use serde::{Deserialize, Serialize};

use crate::{
    Labels, ObjectMeta, Slot, TaskId, TaskPhase, TaskSpec, TaskStatus,
    error::{ModelError, ModelResult},
};

/// Unified task resource.
///
/// Every task is represented as a single resource with three sections:
/// - `status`   - observed state: current phase, attempts, errors ([`TaskStatus`])
/// - `spec`     - desired state: what to run and how ([`TaskSpec`])
/// - `metadata` - identity, versioning, timestamps ([`ObjectMeta`])
///
/// Slot and labels live in `spec` (single source of truth).
/// Convenience accessors [`Task::slot`] and [`Task::labels`] delegate to spec.
///
/// ## Also
///
/// - [`TaskSpec`] desired state (what to run, how to restart).
/// - [`TaskStatus`] observed state (phase, attempt, exit code).
/// - [`TaskRun`](crate::TaskRun) per-attempt execution record.
/// - [`ObjectMeta`] identity and versioning.
/// - [`TaskPhase`] lifecycle state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Resource metadata: identity, versioning, timestamps.
    pub metadata: ObjectMeta,
    /// Observed state: current phase, attempts, errors.
    pub status: TaskStatus,
    /// Desired state: what to run and how.
    pub spec: TaskSpec,
}

impl Task {
    /// Create a new task in [`TaskPhase::Pending`] phase.
    pub fn new(id: TaskId, spec: TaskSpec) -> Self {
        Self {
            metadata: ObjectMeta::new(id),
            status: TaskStatus::pending(),
            spec,
        }
    }

    /// Transition the task into a new attempt: bumps attempt counter, sets phase to `Running`, clears error/exit_code.
    pub fn transition_starting(&mut self) {
        self.increment_attempt();
        self.update_phase(TaskPhase::Running, None, None);
    }

    /// Transition the current attempt into a terminal phase with optional error and exit code.
    ///
    /// Rejects illegal transitions:
    /// - target phase must be terminal (see [`TaskPhase::is_terminal`]);
    ///   finishing into `Pending` or `Running` is a logic bug upstream.
    ///
    /// Bumps `resource_version`.
    pub fn transition_finished(
        &mut self,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> ModelResult<()> {
        if !phase.is_terminal() {
            return Err(ModelError::Invalid(
                format!("transition_finished requires a terminal phase, got {phase}").into(),
            ));
        }
        self.update_phase(phase, error, exit_code);
        Ok(())
    }

    /// Raw phase setter.
    pub(crate) fn update_phase(
        &mut self,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) {
        self.metadata.bump_resource_version();
        self.status.phase = phase;
        self.status.error = error;
        self.status.exit_code = exit_code;
    }

    /// Raw attempt bump. Crate-private (see [`update_phase`]).
    pub(crate) fn increment_attempt(&mut self) {
        self.metadata.bump_resource_version();
        self.status.attempt += 1;
    }

    /// Task identifier (shortcut for `metadata.id`).
    #[inline]
    pub fn id(&self) -> &TaskId {
        &self.metadata.id
    }

    /// Slot (shortcut for `spec.slot()`).
    #[inline]
    pub fn slot(&self) -> &Slot {
        self.spec.slot()
    }

    /// Labels (shortcut for `spec.labels()`).
    #[inline]
    pub fn labels(&self) -> &Labels {
        self.spec.labels()
    }

    /// Current phase (shortcut for `status.phase`).
    #[inline]
    pub fn phase(&self) -> &TaskPhase {
        &self.status.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskKind;

    fn test_spec() -> TaskSpec {
        TaskSpec::builder("slot-a", TaskKind::Embedded, 5_000u64)
            .build()
            .expect("test spec must be valid")
    }

    #[test]
    fn new_creates_pending_task() {
        let task = Task::new("task-1".into(), test_spec());

        assert_eq!(task.status.phase, TaskPhase::Pending);
        assert_eq!(task.metadata.resource_version, 1);
        assert_eq!(task.metadata.id, "task-1");
        assert!(task.status.error.is_none());
        assert_eq!(task.status.attempt, 0);
        assert_eq!(task.slot(), "slot-a");
    }

    #[test]
    fn transition_starting_sets_running_and_bumps() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();

        assert_eq!(task.status.phase, TaskPhase::Running);
        assert_eq!(task.status.attempt, 1);
        assert_eq!(task.metadata.resource_version, 3);
    }

    #[test]
    fn transition_finished_accepts_terminal_and_carries_error() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), Some(1))
            .unwrap();

        assert_eq!(task.status.phase, TaskPhase::Failed);
        assert_eq!(task.status.error.as_deref(), Some("boom"));
        assert_eq!(task.status.exit_code, Some(1));
    }

    #[test]
    fn transition_finished_rejects_non_terminal_phase() {
        let mut task = Task::new("task-1".into(), test_spec());
        let err = task
            .transition_finished(TaskPhase::Running, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("terminal phase"));
    }

    #[test]
    fn convenience_accessors() {
        let spec = TaskSpec::builder("slot-1", TaskKind::Embedded, 5_000u64)
            .build()
            .unwrap();
        let task = Task::new("id-1".into(), spec);

        assert_eq!(task.slot(), &Slot::from("slot-1"));
        assert_eq!(task.id(), &TaskId::from("id-1"));
        assert_eq!(*task.phase(), TaskPhase::Pending);
    }

    #[test]
    fn serde_roundtrip() {
        let spec = TaskSpec::builder("slot-1", TaskKind::Embedded, 5_000u64)
            .build()
            .unwrap();
        let task = Task::new("id-1".into(), spec);
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();

        assert_eq!(back.status.phase, TaskPhase::Pending);
        assert_eq!(back.metadata.resource_version, 1);
        assert_eq!(back.metadata.id, "id-1");
    }
}
