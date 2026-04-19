//! # State event subscriber.
//!
//! [`StateSubscriber`] implements [`Subscribe`](taskvisor::Subscribe) to wire
//! taskvisor lifecycle events into [`TaskState`](super::TaskState) mutations.

use std::sync::Arc;

use taskvisor::{Event, EventKind, Subscribe};
use tracing::{trace, warn};

use super::TaskState;
use solti_model::{TaskId, TaskPhase};

/// Subscriber that updates TaskState from taskvisor events.
///
/// ## Also
///
/// - [`TaskState`](super::TaskState) storage mutated by this subscriber.
/// - [`SupervisorApi::new`](crate::SupervisorApi::new) auto-registers this subscriber.
pub struct StateSubscriber {
    state: TaskState,
}

impl StateSubscriber {
    /// Create a new state subscriber.
    pub fn new(state: TaskState) -> Self {
        Self { state }
    }

    /// Extract TaskId from event, reusing the existing `Arc<str>` allocation.
    fn task_id_from_event(event: &Event) -> Option<TaskId> {
        event.task.as_ref().map(|s| TaskId::from(Arc::clone(s)))
    }
}

impl Subscribe for StateSubscriber {
    fn on_event(&self, event: &Event) {
        let Some(task_id) = Self::task_id_from_event(event) else {
            return;
        };

        match event.kind {
            EventKind::TaskAdded => {
                trace!(task = %task_id, "task added event received (already in state)");
            }
            EventKind::TaskStarting => {
                trace!(task = %task_id, "task starting");
                if self.state.transition_starting(&task_id).is_none() {
                    warn!(task = %task_id, "TaskStarting event for unknown task");
                }
            }
            EventKind::TaskStopped => {
                trace!(task = %task_id, "task stopped (success)");
                if !self
                    .state
                    .transition_finished(&task_id, TaskPhase::Succeeded, None, None)
                {
                    warn!(task = %task_id, "TaskStopped event for unknown task");
                }
            }
            EventKind::TaskFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                trace!(
                    task = %task_id,
                    reason = %reason,
                    exit_code = ?event.exit_code,
                    "task failed",
                );
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Failed,
                    Some(reason),
                    event.exit_code,
                ) {
                    warn!(task = %task_id, "TaskFailed event for unknown task");
                }
            }
            EventKind::TimeoutHit => {
                // Timeouts have no process exit code by definition — the
                // child was killed by us before it could exit.
                trace!(task = %task_id, "task timeout");
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Timeout,
                    Some("timeout".to_string()),
                    None,
                ) {
                    warn!(task = %task_id, "TimeoutHit event for unknown task");
                }
            }
            EventKind::ActorExhausted => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "exhausted".to_string());
                trace!(
                    task = %task_id,
                    exit_code = ?event.exit_code,
                    "task exhausted",
                );
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Exhausted,
                    Some(reason),
                    event.exit_code,
                ) {
                    warn!(task = %task_id, "ActorExhausted event for unknown task");
                }
            }
            // `ActorDead` is published by taskvisor when a task returns
            // `TaskError::Fatal` — the actor is gone and will not be
            // restarted. taskvisor also emits `TaskFailed` just before
            // `ActorDead`, so by this point the phase is already
            // `Failed`. Re-applying the terminal transition here is
            // intentional: it keeps the final `reason`/`exit_code`
            // aligned with the fatal event (subsequent `TaskFailed`
            // wording is per-attempt, while `ActorDead` carries the
            // sealed state), and it guarantees correctness if upstream
            // ever publishes `ActorDead` without a preceding
            // `TaskFailed`.
            EventKind::ActorDead => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "fatal".to_string());
                trace!(
                    task = %task_id,
                    exit_code = ?event.exit_code,
                    "actor dead (fatal)",
                );
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Failed,
                    Some(reason),
                    event.exit_code,
                ) {
                    warn!(task = %task_id, "ActorDead event for unknown task");
                }
            }
            EventKind::TaskRemoved => {
                trace!(task = %task_id, "task removed from state");
                self.state.unregister_task(&task_id);
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "state-subscriber"
    }

    fn queue_capacity(&self) -> usize {
        2048
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{TaskKind, TaskSpec};
    use taskvisor::Event;

    fn test_spec() -> TaskSpec {
        TaskSpec::builder("slot", TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("valid spec")
    }

    fn setup(task_name: &str) -> (StateSubscriber, TaskState, TaskId) {
        let state = TaskState::new();
        let id = TaskId::from(task_name);
        state.add_task(id.clone(), test_spec());
        state.transition_starting(&id);
        let sub = StateSubscriber::new(state.clone());
        (sub, state, id)
    }

    #[test]
    fn actor_dead_seals_phase_as_failed_with_exit_code() {
        let (sub, state, id) = setup("fatal-task");

        let ev = Event::new(EventKind::ActorDead)
            .with_task("fatal-task")
            .with_attempt(3)
            .with_reason("fatal error (no retry): boom")
            .with_exit_code(137);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().exit_code, Some(137));
        assert_eq!(
            task.status().error.as_deref(),
            Some("fatal error (no retry): boom"),
        );
        assert!(task.status().phase.is_terminal());
    }

    #[test]
    fn actor_dead_with_no_exit_code_stores_none() {
        let (sub, state, id) = setup("logical-fatal");

        let ev = Event::new(EventKind::ActorDead)
            .with_task("logical-fatal")
            .with_reason("fatal error (no retry): misconfigured");

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().exit_code, None);
    }

    #[test]
    fn actor_dead_for_unknown_task_is_noop() {
        let state = TaskState::new();
        let sub = StateSubscriber::new(state.clone());

        let ev = Event::new(EventKind::ActorDead)
            .with_task("ghost")
            .with_reason("fatal");

        sub.on_event(&ev);

        assert!(state.get(&TaskId::from("ghost")).is_none());
    }

    #[test]
    fn task_failed_carries_event_exit_code_into_state() {
        let (sub, state, id) = setup("fail-task");

        let ev = Event::new(EventKind::TaskFailed)
            .with_task("fail-task")
            .with_attempt(1)
            .with_reason("execution failed: non-zero")
            .with_exit_code(2);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().exit_code, Some(2));
    }

    #[test]
    fn actor_exhausted_carries_event_exit_code_into_state() {
        let (sub, state, id) = setup("exhausted");

        let ev = Event::new(EventKind::ActorExhausted)
            .with_task("exhausted")
            .with_attempt(5)
            .with_reason("max_retries_exceeded(5/5)")
            .with_exit_code(1);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Exhausted);
        assert_eq!(task.status().exit_code, Some(1));
    }
}
