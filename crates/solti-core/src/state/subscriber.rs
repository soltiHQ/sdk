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

    /// Resolve the task entry an event belongs to.
    ///
    /// Events carrying a taskvisor run identity resolve **only** through the
    /// identity binding (labels are reusable; a late event from a previous
    /// incarnation must not touch the current entry). Id-less events fall back
    /// to the label (synthetic/test events).
    fn resolve(&self, event: &Event) -> Option<TaskId> {
        match event.id {
            Some(tv) => self.state.resolve_tv(tv.get()),
            None => event.task.as_ref().map(|s| TaskId::from(Arc::clone(s))),
        }
    }
}

impl Subscribe for StateSubscriber {
    fn on_event(&self, event: &Event) {
        let Some(task_id) = self.resolve(event) else {
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
            EventKind::TaskCanceled => {
                trace!(task = %task_id, "task canceled (graceful)");
                if !self
                    .state
                    .transition_finished(&task_id, TaskPhase::Canceled, None, None)
                {
                    warn!(task = %task_id, "TaskCanceled event for unknown task");
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
            // Informational: always followed by TaskFailed for the same attempt,
            // which performs the actual transition and announcement.
            EventKind::TimeoutHit => {
                trace!(task = %task_id, "task attempt timed out");
            }
            EventKind::ActorExhausted => {
                // The normal way a task ends: success under OnFailure/Never, or
                // retry budget exhaustion. Success is not an error.
                let reason = event.reason.as_ref().map(|s| s.to_string());
                let is_success = reason.as_deref() == Some("policy_exhausted_success");
                trace!(
                    task = %task_id,
                    exit_code = ?event.exit_code,
                    "actor exhausted",
                );
                let (phase, error) = if is_success {
                    (TaskPhase::Succeeded, None)
                } else {
                    (
                        TaskPhase::Exhausted,
                        Some(reason.unwrap_or_else(|| "exhausted".to_string())),
                    )
                };
                if !self
                    .state
                    .transition_finished(&task_id, phase, error, event.exit_code)
                {
                    warn!(task = %task_id, "ActorExhausted event for unknown task");
                }
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
                self.output_registry.evict(&task_id);
            }
            // Admission rejected: the run will never start. Finalize the
            // provisional entry and release its output channel (the submit()
            // path pre-creates both).
            EventKind::ControllerRejected | EventKind::TaskAddFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "rejected".to_string());
                // User-initiated or shutdown-driven drops are cancellations.
                let phase = match reason.as_str() {
                    "removed_from_queue" | "superseded_by_replace" | "controller_shutting_down" => {
                        TaskPhase::Canceled
                    }
                    _ => TaskPhase::Failed,
                };
                if !self
                    .state
                    .transition_finished(&task_id, phase, Some(reason), None)
                {
                    warn!(task = %task_id, "rejection event for unknown task");
                }
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

    use taskvisor::TaskId as TvId;

    #[test]
    fn late_events_from_previous_incarnation_are_ignored() {
        let (sub, state, id) = setup("reuse-x");
        state.bind_tv(&id, 1);
        // The label was resubmitted: a new incarnation now owns the entry.
        state.bind_tv(&id, 2);

        let stale = Event::new(EventKind::TaskRemoved)
            .with_task("reuse-x")
            .with_id(TvId::from_raw(1));
        sub.on_event(&stale);
        assert!(
            state.get(&id).is_some(),
            "late TaskRemoved from the previous incarnation must be ignored"
        );

        let current = Event::new(EventKind::TaskRemoved)
            .with_task("reuse-x")
            .with_id(TvId::from_raw(2));
        sub.on_event(&current);
        assert!(
            state.get(&id).is_none(),
            "current incarnation's TaskRemoved must unregister"
        );
    }

    #[test]
    fn controller_rejection_finalizes_entry_and_evicts_channel() {
        let state = TaskState::new();
        let id = TaskId::from("rejected-task");
        state.add_task(id.clone(), test_spec()); // provisional Pending entry
        state.bind_tv(&id, 7);
        let registry = Arc::new(OutputRegistry::new(16));
        registry.ensure_channel(id.clone());
        let sub = StateSubscriber::with_output_registry(state.clone(), Arc::clone(&registry));

        // Rejections carry the SLOT name, not the task name: resolution is id-only.
        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("some-slot")
            .with_id(TvId::from_raw(7))
            .with_reason("queue_full: 3/3");
        sub.on_event(&ev);

        let task = state.get(&id).expect("entry kept for observability");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert!(
            task.status()
                .error
                .as_deref()
                .is_some_and(|e| e.contains("queue_full")),
            "rejection reason must be recorded"
        );
        assert!(
            registry.subscribe(&id).is_none(),
            "output channel must be evicted on rejection"
        );
    }

    #[test]
    fn user_initiated_queue_removal_is_canceled_phase() {
        let state = TaskState::new();
        let id = TaskId::from("victim");
        state.add_task(id.clone(), test_spec());
        state.bind_tv(&id, 9);
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );

        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("s")
            .with_id(TvId::from_raw(9))
            .with_reason("removed_from_queue");
        sub.on_event(&ev);

        let task = state.get(&id).expect("entry kept");
        assert_eq!(
            task.status().phase,
            TaskPhase::Canceled,
            "user-initiated removal is a cancellation, not a failure"
        );
    }

    #[test]
    fn task_canceled_event_maps_to_canceled_phase() {
        let (sub, state, id) = setup("graceful");
        state.bind_tv(&id, 3);

        let ev = Event::new(EventKind::TaskCanceled)
            .with_task("graceful")
            .with_id(TvId::from_raw(3))
            .with_attempt(1);
        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Canceled);
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
    fn actor_exhausted_evicts_channel_without_duplicate_run_finished() {
        let (sub, _state, registry, id) = setup_with_registry("exh-evict");
        let mut rx = registry.subscribe(&id).unwrap();

        // The attempt's own terminal event (TaskFailed) announces RunFinished...
        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("exh-evict")
                .with_attempt(5)
                .with_exit_code(1),
        );
        // ...so the actor-terminal event must only evict, not announce again.
        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("exh-evict")
                .with_attempt(5)
                .with_exit_code(1),
        );

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished { attempt, .. } => assert_eq!(attempt, 5),
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "ActorExhausted must not announce a second RunFinished for the same attempt"
        );
        assert!(
            registry.subscribe(&id).is_none(),
            "channel must be evicted on Exhausted"
        );
    }

    #[test]
    fn actor_exhausted_after_success_is_not_an_error() {
        let (sub, state, id) = setup("oneshot");

        sub.on_event(
            &Event::new(EventKind::TaskStopped)
                .with_task("oneshot")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("oneshot")
                .with_attempt(1)
                .with_reason("policy_exhausted_success"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(
            task.status().phase,
            TaskPhase::Succeeded,
            "normal one-shot completion must stay Succeeded"
        );
        assert!(
            task.status().error.is_none(),
            "policy_exhausted_success is not an error"
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
