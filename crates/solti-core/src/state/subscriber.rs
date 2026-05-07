//! # State event subscriber.
//!
//! [`StateSubscriber`] implements [`Subscribe`](taskvisor::Subscribe) to wire
//! taskvisor lifecycle events into [`TaskState`](super::TaskState) mutations.

use std::sync::Arc;

use taskvisor::{Event, EventKind, Subscribe};
use tracing::{trace, warn};

use super::TaskState;
use solti_model::{TaskId, TaskPhase};
use solti_runner::OutputRegistry;

/// Subscriber that updates TaskState from taskvisor events.
///
/// ## Also
///
/// - [`TaskState`](super::TaskState) storage mutated by this subscriber.
/// - [`SupervisorApi::new`](crate::SupervisorApi::new) auto-registers this subscriber.
pub struct StateSubscriber {
    state: TaskState,
    output_registry: Arc<OutputRegistry>,
}

impl StateSubscriber {
    /// Create a state subscriber.
    pub fn with_output_registry(state: TaskState, output_registry: Arc<OutputRegistry>) -> Self {
        Self {
            state,
            output_registry,
        }
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

        let attempt = event.attempt.unwrap_or(0);

        match event.kind {
            EventKind::TaskAdded => {
                trace!(task = %task_id, "task added event received (already in state)");
            }
            EventKind::TaskStarting => {
                trace!(task = %task_id, "task starting");
                if self.state.transition_starting(&task_id).is_none() {
                    warn!(task = %task_id, "TaskStarting event for unknown task");
                }
                self.output_registry.announce_run_started(&task_id, attempt);
            }
            EventKind::TaskStopped => {
                trace!(task = %task_id, "task stopped (success)");
                if !self
                    .state
                    .transition_finished(&task_id, TaskPhase::Succeeded, None, None)
                {
                    warn!(task = %task_id, "TaskStopped event for unknown task");
                }
                self.output_registry
                    .announce_run_finished(&task_id, attempt, None);
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
                self.output_registry
                    .announce_run_finished(&task_id, attempt, event.exit_code);
            }
            EventKind::TimeoutHit => {
                trace!(task = %task_id, "task timeout");
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Timeout,
                    Some("timeout".to_string()),
                    None,
                ) {
                    warn!(task = %task_id, "TimeoutHit event for unknown task");
                }
                self.output_registry
                    .announce_run_finished(&task_id, attempt, None);
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
                self.output_registry
                    .announce_run_finished(&task_id, attempt, event.exit_code);
                self.output_registry.evict(&task_id);
            }
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
                self.output_registry
                    .announce_run_finished(&task_id, attempt, event.exit_code);
                self.output_registry.evict(&task_id);
            }
            EventKind::TaskRemoved => {
                trace!(task = %task_id, "task removed from state");
                self.state.unregister_task(&task_id);
                self.output_registry.evict(&task_id);
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

    use solti_model::{OutputEvent, TaskKind, TaskSpec};
    use solti_runner::OutputRegistry;
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
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(solti_runner::OutputRegistry::default()),
        );
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
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(solti_runner::OutputRegistry::default()),
        );

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

    fn setup_with_registry(
        task_name: &str,
    ) -> (StateSubscriber, TaskState, Arc<OutputRegistry>, TaskId) {
        let state = TaskState::new();
        let id = TaskId::from(task_name);
        state.add_task(id.clone(), test_spec());
        state.transition_starting(&id);
        let registry = Arc::new(OutputRegistry::new(16));
        registry.ensure_channel(id.clone());
        let sub = StateSubscriber::with_output_registry(state.clone(), Arc::clone(&registry));
        (sub, state, registry, id)
    }

    #[test]
    fn task_starting_announces_run_started_into_registry() {
        let (sub, _state, registry, id) = setup_with_registry("started-1");
        let mut rx = registry.subscribe(&id).unwrap();

        let ev = Event::new(EventKind::TaskStarting)
            .with_task("started-1")
            .with_attempt(1);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunStarted { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[test]
    fn task_stopped_announces_run_finished_with_no_exit_code() {
        let (sub, _state, registry, id) = setup_with_registry("stopped-1");
        let mut rx = registry.subscribe(&id).unwrap();

        let ev = Event::new(EventKind::TaskStopped)
            .with_task("stopped-1")
            .with_attempt(2);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                attempt, exit_code, ..
            } => {
                assert_eq!(attempt, 2);
                assert_eq!(exit_code, None);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn task_failed_announces_run_finished_with_exit_code() {
        let (sub, _state, registry, id) = setup_with_registry("failed-1");
        let mut rx = registry.subscribe(&id).unwrap();

        let ev = Event::new(EventKind::TaskFailed)
            .with_task("failed-1")
            .with_attempt(3)
            .with_exit_code(17);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                attempt, exit_code, ..
            } => {
                assert_eq!(attempt, 3);
                assert_eq!(exit_code, Some(17));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn actor_exhausted_announces_run_finished_and_evicts_channel() {
        let (sub, _state, registry, id) = setup_with_registry("exh-evict");
        let mut rx = registry.subscribe(&id).unwrap();

        let ev = Event::new(EventKind::ActorExhausted)
            .with_task("exh-evict")
            .with_attempt(5)
            .with_exit_code(1);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                attempt, exit_code, ..
            } => {
                assert_eq!(attempt, 5);
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert!(
            registry.subscribe(&id).is_none(),
            "channel must be evicted on Exhausted"
        );
    }

    #[test]
    fn actor_dead_announces_run_finished_and_evicts_channel() {
        let (sub, _state, registry, id) = setup_with_registry("dead-evict");

        let ev = Event::new(EventKind::ActorDead)
            .with_task("dead-evict")
            .with_attempt(2)
            .with_exit_code(137);
        sub.on_event(&ev);

        assert!(
            registry.subscribe(&id).is_none(),
            "channel must be evicted on ActorDead"
        );
    }

    #[test]
    fn task_removed_evicts_channel() {
        let (sub, _state, registry, id) = setup_with_registry("remove");

        let ev = Event::new(EventKind::TaskRemoved).with_task("remove");
        sub.on_event(&ev);

        assert!(
            registry.subscribe(&id).is_none(),
            "channel must be evicted on TaskRemoved"
        );
    }
}
