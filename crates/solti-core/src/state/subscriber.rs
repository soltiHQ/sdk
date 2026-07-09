//! # State event subscriber.
//!
//! [`StateSubscriber`] implements [`Subscribe`](taskvisor::Subscribe) and owns two responsibilities driven off taskvisor's lifecycle events:
//! - project events into [`TaskState`](super::TaskState) transitions (phases and `TaskRun` records, including the `TimeoutHit` + `TaskFailed` pairing);
//! - drive the per-run [`OutputRegistry`] lifecycle (announce `RunStarted` / `RunFinished`, evict on terminal).
//!
//! This is the event path. It is fed by taskvisor's best-effort broadcast bus.
//! A dropped terminal event is repaired by the completion path
//! (`finalize_from_outcome`) on [`SupervisorApi`](crate::SupervisorApi).

use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;
use taskvisor::{Event, EventKind, Subscribe};
use tracing::{trace, warn};

use super::TaskState;
use crate::map::phase::{phase_for_exhausted, phase_for_rejection};
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
    timed_out: Mutex<HashMap<TaskId, u32>>,
}

impl StateSubscriber {
    /// Create a state subscriber over a shared [`TaskState`] and [`OutputRegistry`].
    pub fn with_output_registry(state: TaskState, output_registry: Arc<OutputRegistry>) -> Self {
        Self {
            state,
            output_registry,
            timed_out: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the task entry an event belongs to.
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
                let phase = {
                    let mut timed_out = self.timed_out.lock();
                    match timed_out.remove(&task_id) {
                        Some(a) if a == attempt => TaskPhase::Timeout,
                        _ => TaskPhase::Failed,
                    }
                };
                trace!(
                    task = %task_id,
                    reason = %reason,
                    exit_code = ?event.exit_code,
                    phase = %phase,
                    "task failed",
                );
                if !self
                    .state
                    .transition_finished(&task_id, phase, Some(reason), event.exit_code)
                {
                    warn!(task = %task_id, "TaskFailed event for unknown task");
                }
                self.output_registry
                    .announce_run_finished(&task_id, attempt, event.exit_code);
            }
            EventKind::TimeoutHit => {
                trace!(task = %task_id, "task attempt timed out");
                let mut timed_out = self.timed_out.lock();
                timed_out.insert(task_id.clone(), attempt);
                timed_out.retain(|id, _| self.state.contains_task(id));
            }
            EventKind::ActorExhausted => {
                trace!(
                    task = %task_id,
                    exit_code = ?event.exit_code,
                    "actor exhausted",
                );
                let (phase, error) = phase_for_exhausted(event.reason.as_deref());
                if !self
                    .state
                    .transition_finished(&task_id, phase, error, event.exit_code)
                {
                    warn!(task = %task_id, "ActorExhausted event for unknown task");
                }
                self.output_registry.evict(&task_id);
                // The actor is gone for good: release the binding so the entry
                // is not mistaken for a live periodic task if the trailing
                // TaskRemoved is lost to bus lag (the sweep spares bound entries).
                self.state.unbind(&task_id);
                self.timed_out.lock().remove(&task_id);
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
                // Same as ActorExhausted: a dead actor never emits again, so the
                // binding must not keep shielding the entry from the sweep.
                self.state.unbind(&task_id);
                self.timed_out.lock().remove(&task_id);
            }
            EventKind::ControllerRejected | EventKind::TaskAddFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "rejected".to_string());
                let phase = phase_for_rejection(&reason);
                if !self
                    .state
                    .transition_finished(&task_id, phase, Some(reason), None)
                {
                    warn!(task = %task_id, "rejection event for unknown task");
                }
                self.output_registry.evict(&task_id);
                self.state.unbind(&task_id);
                self.timed_out.lock().remove(&task_id);
            }
            EventKind::TaskRemoved => {
                trace!(task = %task_id, "task removed from state");
                self.timed_out.lock().remove(&task_id);
                self.state.unregister_task(&task_id);
                self.output_registry.evict(&task_id);
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "state-subscriber"
    }

    /// Per-subscriber event-queue depth: `2048`, a deliberate 2x of taskvisor's `1024` default.
    fn queue_capacity(&self) -> usize {
        2048
    }
}

#[cfg(test)]
impl StateSubscriber {
    /// Number of pending timeout markers (test-only observability).
    fn timed_out_len(&self) -> usize {
        self.timed_out.lock().len()
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
    fn late_events_from_previous_incarnation_are_ignored() {
        let (sub, state, id) = setup("reuse-x");
        let tvs = [
            taskvisor::TaskId::for_tests(),
            taskvisor::TaskId::for_tests(),
        ];
        state.bind_tv(&id, tvs[0]);
        state.bind_tv(&id, tvs[1]);

        let stale = Event::new(EventKind::TaskRemoved)
            .with_task("reuse-x")
            .with_id(tvs[0]);
        sub.on_event(&stale);
        assert!(
            state.get(&id).is_some(),
            "late TaskRemoved from the previous incarnation must be ignored"
        );

        let current = Event::new(EventKind::TaskRemoved)
            .with_task("reuse-x")
            .with_id(tvs[1]);
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
        state.add_task(id.clone(), test_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        let registry = Arc::new(OutputRegistry::new(16));
        registry.ensure_channel(id.clone());
        let sub = StateSubscriber::with_output_registry(state.clone(), Arc::clone(&registry));

        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("some-slot")
            .with_id(tv)
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
        assert!(
            state.tv_for(&id).is_none(),
            "rejection must release the identity binding so the sweep can reap it"
        );
    }

    #[test]
    fn controller_rejection_unbinds_so_sweep_reaps_the_entry() {
        use crate::state::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = TaskId::from("rej-reap");
        state.add_task(id.clone(), test_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );

        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("slot")
                .with_id(tv)
                .with_reason("queue_full: 3/3"),
        );

        let config = StateConfig {
            run_ttl: Duration::ZERO,
            task_ttl: Duration::ZERO,
            ..StateConfig::default()
        };
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "unbound terminal rejection entry must be swept");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn actor_exhausted_unbinds_so_sweep_reaps_the_entry() {
        use crate::state::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = TaskId::from("exh-reap");
        state.add_task(id.clone(), test_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );

        // The terminal actor event arrives, but the paired TaskRemoved is lost
        // to bus lag: the entry must still become reachable for the sweep.
        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("exh-reap")
                .with_id(tv)
                .with_attempt(1)
                .with_reason("max_retries_exceeded(1/1)"),
        );
        assert!(
            state.tv_for(&id).is_none(),
            "a terminal actor event must release the identity binding"
        );

        let config = StateConfig {
            run_ttl: Duration::ZERO,
            task_ttl: Duration::ZERO,
            ..StateConfig::default()
        };
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "the unbound terminal entry must be reaped");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn actor_dead_unbinds_so_sweep_reaps_the_entry() {
        use crate::state::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = TaskId::from("dead-reap");
        state.add_task(id.clone(), test_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );

        sub.on_event(
            &Event::new(EventKind::ActorDead)
                .with_task("dead-reap")
                .with_id(tv)
                .with_attempt(1)
                .with_reason("fatal error (no retry): boom"),
        );
        assert!(
            state.tv_for(&id).is_none(),
            "ActorDead must release the identity binding"
        );

        let config = StateConfig {
            run_ttl: Duration::ZERO,
            task_ttl: Duration::ZERO,
            ..StateConfig::default()
        };
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "the unbound terminal entry must be reaped");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn user_initiated_queue_removal_is_canceled_phase() {
        let state = TaskState::new();
        let id = TaskId::from("victim");
        state.add_task(id.clone(), test_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );

        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("s")
            .with_id(tv)
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
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);

        let ev = Event::new(EventKind::TaskCanceled)
            .with_task("graceful")
            .with_id(tv)
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

        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("exh-evict")
                .with_attempt(5)
                .with_exit_code(1),
        );
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
    fn timeout_hit_then_task_failed_for_same_attempt_maps_to_timeout_phase() {
        let (sub, state, id) = setup("slow-task");

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("slow-task")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("slow-task")
                .with_attempt(1)
                .with_reason("deadline exceeded"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase, TaskPhase::Timeout);
    }

    #[test]
    fn pending_timeout_is_cleared_when_actor_terminates_without_paired_failed() {
        let (sub, _state, _id) = setup("timeout-orphan");

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("timeout-orphan")
                .with_attempt(1),
        );
        assert_eq!(sub.timed_out_len(), 1);

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("timeout-orphan")
                .with_attempt(1)
                .with_reason("max_retries_exceeded"),
        );
        assert_eq!(
            sub.timed_out_len(),
            0,
            "a pending timeout marker must be cleared when the actor terminates"
        );
    }

    #[test]
    fn timeout_hit_prunes_markers_for_tasks_no_longer_in_state() {
        let state = TaskState::new();
        let sub = StateSubscriber::with_output_registry(
            state.clone(),
            Arc::new(OutputRegistry::default()),
        );
        let gone = TaskId::from("gone");
        let live = TaskId::from("live");
        state.add_task(gone.clone(), test_spec());
        state.add_task(live.clone(), test_spec());

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("gone")
                .with_attempt(1),
        );
        assert_eq!(sub.timed_out_len(), 1);

        state.unregister_task(&gone);

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("live")
                .with_attempt(1),
        );
        assert_eq!(
            sub.timed_out_len(),
            1,
            "the dead task's marker is pruned; only the live task's remains"
        );
    }

    #[test]
    fn task_failed_without_timeout_hit_stays_failed() {
        let (sub, state, id) = setup("plain-fail");

        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("plain-fail")
                .with_attempt(1)
                .with_reason("boom"),
        );

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Failed);
    }

    #[test]
    fn actor_exhausted_task_returned_canceled_maps_to_canceled() {
        let (sub, state, id) = setup("self-cancel");

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("self-cancel")
                .with_attempt(1)
                .with_reason("task_returned_canceled"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(
            task.status().phase,
            TaskPhase::Canceled,
            "task_returned_canceled is a cancellation, not an exhaustion"
        );
        assert!(task.status().error.is_none());
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
