//! Task resource.
//!
//! [`Task`] combines metadata, desired spec, and observed status.

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
/// ## Example
///
/// ```
/// use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};
///
/// let spec = TaskSpec::builder("build", TaskKind::Embedded, 1_000u64)
///     .build()
///     .unwrap();
/// let task = Task::new(TaskId::from("embedded-build-1"), spec);
///
/// assert_eq!(*task.phase(), TaskPhase::Pending);
/// assert_eq!(task.slot().as_str(), "build");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    metadata: ObjectMeta,
    status: TaskStatus,
    spec: TaskSpec,
}

impl Task {
    /// Create a new task in [`TaskPhase::Pending`] phase.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};
    ///
    /// let spec = TaskSpec::builder("build", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap();
    /// let task = Task::new(TaskId::from("task-1"), spec);
    ///
    /// assert_eq!(*task.phase(), TaskPhase::Pending);
    /// assert_eq!(task.metadata().resource_version, 1);
    /// ```
    pub fn new(id: TaskId, spec: TaskSpec) -> Self {
        Self {
            metadata: ObjectMeta::new(id),
            status: TaskStatus::pending(),
            spec,
        }
    }

    /// Resource metadata (identity, `resource_version`, timestamps).
    #[inline]
    pub fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    /// Observed state (phase, attempt, exit code, error).
    #[inline]
    pub fn status(&self) -> &TaskStatus {
        &self.status
    }

    /// Desired state (what to run, how to restart).
    #[inline]
    pub fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// Destructure into `(metadata, spec, status)`.
    ///
    /// Transport layers use this when they need owned fields for wire types.
    #[inline]
    pub fn into_parts(self) -> (ObjectMeta, TaskSpec, TaskStatus) {
        (self.metadata, self.spec, self.status)
    }

    /// Transition the task into a new running attempt.
    ///
    /// This bumps the attempt counter, sets phase to `Running`, clears `error` and `exit_code`, and bumps `resource_version`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};
    ///
    /// let spec = TaskSpec::builder("build", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap();
    /// let mut task = Task::new(TaskId::from("task-1"), spec);
    ///
    /// task.transition_starting();
    ///
    /// assert_eq!(*task.phase(), TaskPhase::Running);
    /// assert_eq!(task.status().attempt, 1);
    /// ```
    pub fn transition_starting(&mut self) {
        self.increment_attempt();
        self.update_phase(TaskPhase::Running, None, None);
    }

    /// Transition the current attempt into a terminal phase.
    ///
    /// Rejects illegal transitions:
    /// - target phase must be terminal (see [`TaskPhase::is_terminal`]);
    ///   finishing into `Pending` or `Running` is a logic bug upstream.
    ///
    /// Sticky terminals:
    /// once an attempt has reached a final state, a later terminal event must not overwrite it.
    ///
    /// taskvisor has two event levels:
    /// - attempt events: `TaskStopped`, `TaskCanceled`, `TaskFailed`;
    /// - actor events: `ActorExhausted`, `ActorDead`.
    ///
    /// Some flows produce both levels for the same attempt.
    /// For example, `TimeoutHit` + `TaskFailed` can mark an attempt as `Timeout`, and a later `ActorExhausted` may report that the actor is done.
    /// Without this guard, that later actor event could replace the more specific attempt result.
    ///
    /// Exactly one refinement is allowed:
    /// the generic failure phase (`Failed`) may be refined into a more specific disposition (`Exhausted`/`Timeout`).
    /// Re-applying the current phase is a harmless no-op that keeps the recorded error/exit code;
    /// every other conflicting terminal event is silently ignored (`Ok(())`).
    ///
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};
    ///
    /// let spec = TaskSpec::builder("build", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap();
    /// let mut task = Task::new(TaskId::from("task-1"), spec);
    ///
    /// task.transition_starting();
    /// task.transition_finished(TaskPhase::Failed, Some("boom".into()), Some(1))?;
    ///
    /// assert_eq!(*task.phase(), TaskPhase::Failed);
    /// assert_eq!(task.status().error.as_deref(), Some("boom"));
    /// # Ok::<(), solti_model::ModelError>(())
    /// ```
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
        if self.status.phase == phase {
            return Ok(());
        }
        if self.status.phase.is_terminal() {
            let refines_failed = self.status.phase == TaskPhase::Failed
                && matches!(phase, TaskPhase::Exhausted | TaskPhase::Timeout);
            if !refines_failed {
                return Ok(());
            }
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

        assert_eq!(task.status().phase, TaskPhase::Pending);
        assert_eq!(task.metadata().resource_version, 1);
        assert_eq!(task.metadata().id, "task-1");
        assert!(task.status().error.is_none());
        assert_eq!(task.status().attempt, 0);
        assert_eq!(task.slot(), "slot-a");
    }

    #[test]
    fn transition_starting_sets_running_and_bumps() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();

        assert_eq!(task.status().phase, TaskPhase::Running);
        assert_eq!(task.status().attempt, 1);
        assert_eq!(task.metadata().resource_version, 3);
    }

    #[test]
    fn transition_finished_accepts_terminal_and_carries_error() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), Some(1))
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().error.as_deref(), Some("boom"));
        assert_eq!(task.status().exit_code, Some(1));
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
    fn canceled_is_not_overwritten_by_a_later_terminal_event() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Canceled, None, None)
            .unwrap();

        task.transition_finished(TaskPhase::Exhausted, Some("budget".into()), Some(1))
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Canceled);
        assert!(task.status().error.is_none());
        assert_eq!(task.status().exit_code, None);
    }

    #[test]
    fn succeeded_is_not_overwritten_by_a_later_terminal_event() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Succeeded, None, None)
            .unwrap();

        task.transition_finished(TaskPhase::Failed, Some("late".into()), Some(2))
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Succeeded);
        assert!(task.status().error.is_none());
    }

    #[test]
    fn timeout_is_not_overwritten_by_a_later_terminal_event() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Timeout, Some("deadline".into()), None)
            .unwrap();

        task.transition_finished(TaskPhase::Exhausted, Some("budget".into()), None)
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Timeout);
    }

    #[test]
    fn failed_can_be_refined_to_exhausted() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("attempt".into()), None)
            .unwrap();

        task.transition_finished(TaskPhase::Exhausted, Some("max_retries".into()), None)
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Exhausted);
        assert_eq!(task.status().error.as_deref(), Some("max_retries"));
    }

    #[test]
    fn failed_can_be_refined_to_timeout() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("attempt".into()), None)
            .unwrap();

        task.transition_finished(TaskPhase::Timeout, Some("deadline".into()), None)
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Timeout);
        assert_eq!(task.status().error.as_deref(), Some("deadline"));
    }

    #[test]
    fn exhausted_survives_late_failed() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Exhausted, Some("max_retries".into()), Some(1))
            .unwrap();
        let version = task.metadata().resource_version;

        task.transition_finished(TaskPhase::Failed, Some("late".into()), Some(2))
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Exhausted);
        assert_eq!(task.status().error.as_deref(), Some("max_retries"));
        assert_eq!(task.status().exit_code, Some(1));
        assert_eq!(task.metadata().resource_version, version);
    }

    #[test]
    fn failed_cannot_become_succeeded_or_canceled() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), Some(1))
            .unwrap();
        let version = task.metadata().resource_version;

        task.transition_finished(TaskPhase::Succeeded, None, None)
            .unwrap();
        assert_eq!(task.status().phase, TaskPhase::Failed);

        task.transition_finished(TaskPhase::Canceled, None, None)
            .unwrap();
        assert_eq!(task.status().phase, TaskPhase::Failed);

        assert_eq!(task.status().error.as_deref(), Some("boom"));
        assert_eq!(task.status().exit_code, Some(1));
        assert_eq!(task.metadata().resource_version, version);
    }

    #[test]
    fn same_phase_reapply_is_noop() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("first".into()), Some(1))
            .unwrap();
        let version = task.metadata().resource_version;

        task.transition_finished(TaskPhase::Failed, Some("second".into()), None)
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().error.as_deref(), Some("first"));
        assert_eq!(task.status().exit_code, Some(1));
        assert_eq!(task.metadata().resource_version, version);
    }

    #[test]
    fn same_phase_reapply_is_noop_for_sticky_terminals() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Canceled, Some("graceful".into()), None)
            .unwrap();
        let version = task.metadata().resource_version;

        task.transition_finished(TaskPhase::Canceled, None, None)
            .unwrap();

        assert_eq!(task.status().phase, TaskPhase::Canceled);
        assert_eq!(task.status().error.as_deref(), Some("graceful"));
        assert_eq!(task.metadata().resource_version, version);
    }

    #[test]
    fn transition_starting_still_leaves_terminal_phase() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.transition_starting();
        task.transition_finished(TaskPhase::Failed, Some("boom".into()), Some(1))
            .unwrap();

        task.transition_starting();

        assert_eq!(task.status().phase, TaskPhase::Running);
        assert_eq!(task.status().attempt, 2);
        assert!(task.status().error.is_none());
        assert_eq!(task.status().exit_code, None);
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

        assert_eq!(back.status().phase, TaskPhase::Pending);
        assert_eq!(back.metadata().resource_version, 1);
        assert_eq!(back.metadata().id, "id-1");
    }
}
