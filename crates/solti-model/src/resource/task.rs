use serde::{Deserialize, Serialize};

use crate::{Labels, ObjectMeta, Slot, TaskId, TaskPhase, TaskSpec, TaskStatus};

/// Unified task resource.
///
/// Every task is represented as a single resource with three sections:
/// - `status`   - observed state: current phase, attempts, errors ([`TaskStatus`])
/// - `spec`     - desired state: what to run and how ([`TaskSpec`])
/// - `metadata` - identity, versioning, timestamps ([`ObjectMeta`])
///
/// Slot and labels live in `spec` (single source of truth).
/// Convenience accessors [`Task::slot`] and [`Task::labels`] delegate to spec.
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

    /// Transition phase with optional error and exit code.
    ///
    /// Bumps `resource_version`.
    pub fn update_phase(
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

    /// Increment attempt counter. Bumps `resource_version`.
    pub fn increment_attempt(&mut self) {
        self.metadata.bump_resource_version();
        self.status.attempt += 1;
    }

    /// Task identifier (shortcut for `metadata.id`).
    #[inline]
    pub fn id(&self) -> &TaskId {
        &self.metadata.id
    }

    /// Slot (shortcut for `spec.slot`).
    #[inline]
    pub fn slot(&self) -> &Slot {
        &self.spec.slot
    }

    /// Labels (shortcut for `spec.labels`).
    #[inline]
    pub fn labels(&self) -> &Labels {
        &self.spec.labels
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
    use crate::{AdmissionPolicy, BackoffPolicy, Labels, RestartPolicy, TaskKind};

    fn test_spec() -> TaskSpec {
        TaskSpec {
            timeout: 5_000_u64.into(),
            slot: "slot-a".into(),

            kind: TaskKind::Embedded,
            runner_selector: None,
            labels: Labels::new(),

            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            admission: AdmissionPolicy::default(),
        }
    }

    #[test]
    fn new_creates_pending_task() {
        let task = Task::new("task-1".into(), test_spec());

        assert_eq!(task.status.phase, TaskPhase::Pending);
        assert_eq!(task.metadata.resource_version, 1);
        assert_eq!(task.metadata.generation, 1);
        assert_eq!(task.metadata.id, "task-1");
        assert!(task.status.error.is_none());
        assert_eq!(task.status.attempt, 0);
        assert_eq!(task.slot(), "slot-a");
    }

    #[test]
    fn update_phase_bumps_resource_version() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.update_phase(TaskPhase::Running, None, None);

        assert_eq!(task.status.phase, TaskPhase::Running);
        assert_eq!(task.metadata.resource_version, 2);
        assert_eq!(task.metadata.generation, 1);
    }

    #[test]
    fn update_phase_with_error() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.update_phase(TaskPhase::Failed, Some("boom".into()), Some(1));

        assert_eq!(task.status.error.as_deref(), Some("boom"));
        assert_eq!(task.status.phase, TaskPhase::Failed);
        assert_eq!(task.status.exit_code, Some(1));
    }

    #[test]
    fn increment_attempt_bumps_resource_version() {
        let mut task = Task::new("task-1".into(), test_spec());
        task.increment_attempt();

        assert_eq!(task.metadata.resource_version, 2);
        assert_eq!(task.status.attempt, 1);
    }

    #[test]
    fn convenience_accessors() {
        let mut spec = test_spec();
        spec.slot = "slot-1".into();
        let task = Task::new("id-1".into(), spec);

        assert_eq!(task.slot(), &Slot::from("slot-1"));
        assert_eq!(task.id(), &TaskId::from("id-1"));
        assert_eq!(*task.phase(), TaskPhase::Pending);
    }

    #[test]
    fn serde_roundtrip() {
        let mut spec = test_spec();
        spec.slot = "slot-1".into();
        let task = Task::new("id-1".into(), spec);
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();

        assert_eq!(back.status.phase, TaskPhase::Pending);
        assert_eq!(back.metadata.generation, 1);
        assert_eq!(back.metadata.id, "id-1");
    }
}
