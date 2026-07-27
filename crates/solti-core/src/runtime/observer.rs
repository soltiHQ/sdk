//! # Runtime observer.
//!
//! [`RuntimeObserver`] implements [`Subscribe`](taskvisor::Subscribe) and owns three responsibilities driven off taskvisor's lifecycle events:
//! - project attempt events into [`TaskState`](crate::TaskState) transitions and `TaskRun` records;
//! - project typed `TaskFinished` outcomes into the resource-level terminal phase;
//! - drive per-run output announcements (`RunStarted` / `RunFinished`).
//!
//! This is the event path. It is fed by taskvisor's best-effort broadcast bus.
//! The direct completion outcome repairs a dropped terminal event. For a
//! registered task, `TaskRemoved` normally acts as a per-subscriber FIFO
//! barrier before binding/output cleanup. The direct outcome finalizes after a
//! bounded wait if that barrier is lost or delayed. Per-attempt detail
//! remains best-effort.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use taskvisor::{Event, EventKind, Subscribe};
use tokio::sync::watch;
use tracing::{trace, warn};

use crate::map::phase::{phase_for_outcome, phase_for_outcome_kind, phase_for_rejection};
use crate::output::OutputHub;
use crate::state::{ResourceGeneration, RuntimeBinding, TaskState};
use solti_model::{TaskId, TaskPhase};

const RUNTIME_OBSERVER_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2048).unwrap();
const REMOVED_ID_CAPACITY: usize = 4096;
const EVENT_BARRIER_TIMEOUT: Duration = Duration::from_secs(1);

/// Serializes short state/output lifecycle commits across event, waiter, and
/// management paths.
#[derive(Clone, Default)]
struct LifecycleGate {
    inner: Arc<Mutex<()>>,
}

impl LifecycleGate {
    fn lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.inner.lock()
    }
}

#[derive(Clone)]
struct Finalization {
    phase: TaskPhase,
    error: Option<String>,
    exit_code: Option<i32>,
    force: bool,
    safe_without_barrier: bool,
}

#[derive(Default)]
struct CompletionBarriers {
    pending: HashMap<u64, Finalization>,
    removed: HashSet<u64>,
    removed_order: VecDeque<u64>,
    notifications: HashMap<u64, watch::Sender<bool>>,
}

impl CompletionBarriers {
    fn notification(&mut self, id: u64) -> watch::Receiver<bool> {
        self.notifications
            .entry(id)
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    fn notify_finalized(&mut self, id: u64) {
        if let Some(tx) = self.notifications.remove(&id) {
            tx.send_replace(true);
        }
    }

    fn mark_removed(&mut self, id: u64) {
        if self.removed.insert(id) {
            self.removed_order.push_back(id);
        }
        while self.removed.len() > REMOVED_ID_CAPACITY {
            let Some(oldest) = self.removed_order.pop_front() else {
                self.removed.clear();
                break;
            };
            self.removed.remove(&oldest);
        }
    }

    fn take_removed(&mut self, id: u64) -> bool {
        let removed = self.removed.remove(&id);
        if removed {
            self.removed_order.retain(|removed_id| *removed_id != id);
        }
        removed
    }
}

/// Subscriber that updates TaskState from taskvisor events.
///
/// ## Also
///
/// - [`TaskState`](crate::TaskState) stores the projected state.
/// - [`SupervisorApiBuilder`](crate::SupervisorApiBuilder) installs this observer.
pub(crate) struct RuntimeObserver {
    state: TaskState,
    output_hub: Arc<OutputHub>,
    lifecycle_gate: LifecycleGate,
    completion_barriers: Mutex<CompletionBarriers>,
}

impl RuntimeObserver {
    /// Create a runtime observer over shared task state and output hub.
    pub(crate) fn with_output_hub(state: TaskState, output_hub: Arc<OutputHub>) -> Self {
        Self {
            state,
            output_hub,
            lifecycle_gate: LifecycleGate::default(),
            completion_barriers: Mutex::new(CompletionBarriers::default()),
        }
    }

    /// Bind a prepared controller submission before it can publish events.
    ///
    /// Returns `false` when the resource UID or generation is no longer current.
    pub(crate) fn bind(
        &self,
        resource: ResourceGeneration,
        tv: taskvisor::TaskId,
        ensure_output: bool,
    ) -> bool {
        let _lifecycle = self.lifecycle_gate.lock();
        let task_id = resource.name.clone();
        if !self.state.bind_tv(resource, tv) {
            return false;
        }
        if ensure_output {
            self.output_hub.ensure_channel_if_absent(task_id);
        }
        true
    }

    /// Release one exact failed intake binding and retain its desired generation
    /// with a reconciliation failure status.
    pub(crate) fn fail_bound_reconciliation(
        &self,
        binding: &RuntimeBinding,
        reason: &'static str,
        message: String,
    ) -> bool {
        let _lifecycle = self.lifecycle_gate.lock();
        if self.state.resolve_tv(binding.tv.get()).as_ref() != Some(binding) {
            return false;
        }

        self.state.unbind_tv(binding.tv.get());
        self.output_hub.evict(&binding.resource.name);
        let changed = self
            .state
            .mark_reconciliation_failed(&binding.resource, reason, message);
        self.mark_completed_locked(binding.tv.get());
        changed
    }

    /// Finalize one current binding from the direct completion channel.
    ///
    /// For registered tasks, `TaskRemoved` is used as a FIFO barrier so
    /// earlier per-attempt events normally reach state before identity/output
    /// cleanup. The direct outcome finalizes after a bounded wait when that
    /// best-effort barrier is lost or delayed.
    pub(crate) async fn finalize_from_outcome(
        &self,
        tv_raw: u64,
        outcome: &taskvisor::TaskOutcome,
    ) {
        use taskvisor::TaskOutcome;

        let (phase, error, exit_code) = phase_for_outcome(outcome);
        let rejected = matches!(outcome, TaskOutcome::Rejected { .. });
        let finalization = Finalization {
            phase,
            error,
            exit_code,
            force: Self::is_known_outcome(outcome),
            safe_without_barrier: true,
        };
        self.finalize_with_event_barrier(tv_raw, finalization, !rejected)
            .await;
    }

    fn is_known_outcome(outcome: &taskvisor::TaskOutcome) -> bool {
        use taskvisor::TaskOutcome;

        matches!(
            outcome,
            TaskOutcome::Completed
                | TaskOutcome::Failed { .. }
                | TaskOutcome::Fatal { .. }
                | TaskOutcome::Canceled
                | TaskOutcome::ForceAborted
                | TaskOutcome::Panicked
                | TaskOutcome::Rejected { .. }
        )
    }

    async fn finalize_with_event_barrier(
        &self,
        tv_raw: u64,
        finalization: Finalization,
        wait_for_barrier: bool,
    ) {
        let notification = {
            let _lifecycle = self.lifecycle_gate.lock();
            self.register_finalization_locked(tv_raw, finalization, wait_for_barrier)
        };

        if let Some(notification) = notification
            && !Self::wait_for_finalization(notification).await
        {
            let _lifecycle = self.lifecycle_gate.lock();
            self.force_pending_locked(tv_raw);
        }
    }

    /// Finalize an unavailable outcome after identity cleanup was confirmed.
    pub(crate) async fn finalize_unavailable_after_cleanup(&self, tv_raw: u64, error: String) {
        self.finalize_with_event_barrier(
            tv_raw,
            Finalization {
                phase: TaskPhase::Failed,
                error: Some(error),
                exit_code: None,
                force: false,
                safe_without_barrier: true,
            },
            true,
        )
        .await;
    }

    /// Preserve a waiter failure until a `TaskRemoved` barrier proves cleanup.
    pub(crate) fn finalize_unavailable(&self, tv_raw: u64, error: String) {
        let finalization = Finalization {
            phase: TaskPhase::Failed,
            error: Some(error),
            exit_code: None,
            force: false,
            safe_without_barrier: false,
        };
        let _lifecycle = self.lifecycle_gate.lock();
        self.register_finalization_locked(tv_raw, finalization, true);
    }

    /// Settle the exact local binding after taskvisor confirmed identity cleanup.
    ///
    /// The tracked direct-completion task owns the authoritative final outcome.
    /// Waiting for it here keeps cancel and immediate same-name resubmission
    /// linearizable without replacing a delayed real outcome with a synthetic
    /// phase.
    pub(crate) async fn settle_after_confirmed_cleanup(&self, tv: taskvisor::TaskId) {
        let tv_raw = tv.get();
        let mut notification = {
            let _lifecycle = self.lifecycle_gate.lock();
            if self.state.resolve_tv(tv_raw).is_none() {
                return;
            }
            self.completion_barriers.lock().notification(tv_raw)
        };

        while !*notification.borrow() {
            if notification.changed().await.is_err() {
                return;
            }
        }
    }

    /// Delete a local resource after taskvisor cancellation/removal is settled.
    pub(crate) fn delete_after_cleanup(&self, id: &TaskId, tv: Option<taskvisor::TaskId>) -> bool {
        let _lifecycle = self.lifecycle_gate.lock();
        let removed = self.state.delete_task(id);
        if removed || tv.is_some() {
            self.output_hub.evict(id);
        }
        if let Some(tv) = tv {
            self.mark_completed_locked(tv.get());
        }
        removed
    }

    async fn wait_for_finalization(mut notification: watch::Receiver<bool>) -> bool {
        if *notification.borrow() {
            return true;
        }
        matches!(
            tokio::time::timeout(EVENT_BARRIER_TIMEOUT, notification.changed()).await,
            Ok(Ok(()))
        ) && *notification.borrow()
    }

    fn register_finalization_locked(
        &self,
        tv_raw: u64,
        finalization: Finalization,
        wait_for_barrier: bool,
    ) -> Option<watch::Receiver<bool>> {
        self.state.resolve_tv(tv_raw)?;

        let (finalize_now, notification) = {
            let mut barriers = self.completion_barriers.lock();
            if !wait_for_barrier || barriers.take_removed(tv_raw) {
                (Some(finalization), None)
            } else {
                match barriers.pending.get_mut(&tv_raw) {
                    Some(pending)
                        if !pending.safe_without_barrier && finalization.safe_without_barrier =>
                    {
                        *pending = finalization;
                    }
                    Some(_) => {}
                    None => {
                        barriers.pending.insert(tv_raw, finalization);
                    }
                }
                (None, Some(barriers.notification(tv_raw)))
            }
        };

        if let Some(finalization) = finalize_now {
            self.finalize_locked(tv_raw, finalization);
        }
        notification
    }

    fn force_pending_locked(&self, tv_raw: u64) {
        let pending = {
            let mut barriers = self.completion_barriers.lock();
            if barriers
                .pending
                .get(&tv_raw)
                .is_some_and(|pending| pending.safe_without_barrier)
            {
                barriers.pending.remove(&tv_raw)
            } else {
                None
            }
        };
        if let Some(finalization) = pending {
            self.finalize_locked(tv_raw, finalization);
        }
    }

    fn force_all_safe_pending_locked(&self) {
        let pending = {
            let mut barriers = self.completion_barriers.lock();
            let ids: Vec<u64> = barriers
                .pending
                .iter()
                .filter_map(|(id, pending)| pending.safe_without_barrier.then_some(*id))
                .collect();
            ids.into_iter()
                .filter_map(|id| barriers.pending.remove(&id).map(|pending| (id, pending)))
                .collect::<Vec<_>>()
        };
        for (tv_raw, finalization) in pending {
            self.finalize_locked(tv_raw, finalization);
        }
    }

    fn task_removed_locked(&self, tv_raw: u64) {
        let pending = {
            let mut barriers = self.completion_barriers.lock();
            let pending = barriers.pending.remove(&tv_raw);
            if pending.is_none() {
                barriers.mark_removed(tv_raw);
            }
            pending
        };
        if let Some(finalization) = pending {
            self.finalize_locked(tv_raw, finalization);
        }
    }

    fn finalize_locked(&self, tv_raw: u64, finalization: Finalization) {
        if let Some(model_id) = self.state.finalize_if_bound(
            tv_raw,
            finalization.phase,
            finalization.error,
            finalization.exit_code,
            finalization.force,
        ) {
            self.output_hub.evict(&model_id);
        }
        self.mark_completed_locked(tv_raw);
    }

    fn mark_completed_locked(&self, tv_raw: u64) {
        let mut barriers = self.completion_barriers.lock();
        barriers.pending.remove(&tv_raw);
        barriers.take_removed(tv_raw);
        barriers.notify_finalized(tv_raw);
    }

    /// Resolve the task entry an event belongs to.
    fn resolve(&self, event: &Event) -> Option<RuntimeBinding> {
        self.state.resolve_tv(event.id?.get())
    }
}

impl RuntimeObserver {
    /// Apply one event while the shared lifecycle gate is held.
    fn apply_event_locked(&self, event: &Event) {
        let Some(tv) = event.id else {
            return;
        };
        let tv_raw = tv.get();

        let Some(binding) = self.resolve(event) else {
            return;
        };
        let task_id = &binding.resource.name;

        // Output cleanup does not own the retained SDK resource. The
        // direct completion finalizes it; explicit delete removes it eagerly.
        if event.kind == EventKind::TaskRemoved {
            self.task_removed_locked(tv_raw);
            return;
        }

        match event.kind {
            EventKind::TaskAdded => {
                trace!(task = %task_id, "task added event received (already in state)");
            }
            EventKind::AttemptStarting => {
                let Some(attempt) = event.attempt else {
                    warn!(task = %task_id, "AttemptStarting event has no attempt");
                    return;
                };
                trace!(task = %task_id, "task attempt starting");
                if self.state.transition_attempt_starting(&binding, attempt) {
                    self.output_hub.announce_run_started(
                        task_id,
                        binding.resource.generation,
                        attempt,
                    );
                } else {
                    warn!(task = %task_id, "AttemptStarting event for stale task generation");
                }
            }
            EventKind::AttemptSucceeded => {
                let Some(attempt) = event.attempt else {
                    warn!(task = %task_id, "AttemptSucceeded event has no attempt");
                    return;
                };
                trace!(task = %task_id, "task attempt succeeded");
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Succeeded,
                    None,
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(task = %task_id, "AttemptSucceeded event for stale task generation");
                }
            }
            EventKind::AttemptCanceled => {
                let Some(attempt) = event.attempt else {
                    warn!(task = %task_id, "AttemptCanceled event has no attempt");
                    return;
                };
                trace!(task = %task_id, "task attempt canceled cooperatively");
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Canceled,
                    None,
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(task = %task_id, "AttemptCanceled event for stale task generation");
                }
            }
            EventKind::AttemptFailed => {
                let Some(attempt) = event.attempt else {
                    warn!(task = %task_id, "AttemptFailed event has no attempt");
                    return;
                };
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                trace!(
                    task = %task_id,
                    reason = %reason,
                    exit_code = ?event.exit_code,
                    "task attempt failed",
                );
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Failed,
                    Some(reason),
                    event.exit_code,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        binding.resource.generation,
                        attempt,
                        event.exit_code,
                    );
                } else {
                    warn!(task = %task_id, "AttemptFailed event for stale task generation");
                }
            }
            EventKind::AttemptTimedOut => {
                let Some(attempt) = event.attempt else {
                    warn!(task = %task_id, "AttemptTimedOut event has no attempt");
                    return;
                };
                let error = event.timeout_ms.map_or_else(
                    || "task attempt timed out".to_string(),
                    |timeout_ms| format!("task attempt timed out after {timeout_ms} ms"),
                );
                trace!(task = %task_id, "task attempt timed out");
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Timeout,
                    Some(error),
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(task = %task_id, "AttemptTimedOut event for stale task generation");
                }
            }
            EventKind::TaskFinished => {
                let Some(outcome_kind) = event.outcome_kind else {
                    warn!(task = %task_id, "TaskFinished event has no outcome_kind");
                    return;
                };
                let (phase, error, exit_code) =
                    phase_for_outcome_kind(outcome_kind, event.reason.as_deref(), event.exit_code);
                trace!(
                    task = %task_id,
                    outcome = outcome_kind.as_label(),
                    exit_code = ?exit_code,
                    "task reached final outcome",
                );
                if !self
                    .state
                    .transition_task_finished(&binding, phase, error, exit_code)
                {
                    warn!(task = %task_id, "TaskFinished event for stale task generation");
                }
            }
            EventKind::ControllerRejected => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "rejected".to_string());
                let phase = event
                    .rejection_kind
                    .map(phase_for_rejection)
                    .unwrap_or(TaskPhase::Failed);
                if !self
                    .state
                    .transition_task_finished(&binding, phase, Some(reason), None)
                {
                    warn!(task = %task_id, "rejection event for stale task generation");
                }
            }
            EventKind::TaskAddFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "task add failed".to_string());
                let phase = event
                    .rejection_kind
                    .map(phase_for_rejection)
                    .unwrap_or(TaskPhase::Failed);
                if !self
                    .state
                    .transition_task_finished(&binding, phase, Some(reason), None)
                {
                    warn!(task = %task_id, "TaskAddFailed event for stale task generation");
                }
            }
            _ => {}
        }
    }
}

impl Subscribe for RuntimeObserver {
    fn on_event(&self, event: &Event) {
        let _lifecycle = self.lifecycle_gate.lock();
        if event.kind == EventKind::SubscriberOverflow {
            if event.task.as_deref().is_some_and(|subscriber| {
                subscriber != self.name() && subscriber != "subscriber_listener"
            }) {
                return;
            }
            self.force_all_safe_pending_locked();
            return;
        }

        self.apply_event_locked(event);
    }

    fn name(&self) -> &'static str {
        "state-subscriber"
    }

    /// Per-subscriber event-queue depth: `2048`, a deliberate 2x of taskvisor's `1024` default.
    fn queue_capacity(&self) -> NonZeroUsize {
        RUNTIME_OBSERVER_QUEUE_CAPACITY
    }
}

#[cfg(test)]
impl RuntimeObserver {
    pub(crate) fn finalize_outcome_immediately_for_test(
        &self,
        tv_raw: u64,
        outcome: &taskvisor::TaskOutcome,
    ) {
        let _lifecycle = self.lifecycle_gate.lock();
        let (phase, error, exit_code) = phase_for_outcome(outcome);
        self.finalize_locked(
            tv_raw,
            Finalization {
                phase,
                error,
                exit_code,
                force: Self::is_known_outcome(outcome),
                safe_without_barrier: true,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskvisor::TaskOutcomeKind;

    use crate::output::{OutputConfig, OutputHub};
    use solti_model::{
        ConditionStatus, EmbeddedSpec, OutputEvent, TaskManifest, TaskSpec, TaskWorkload,
    };
    use taskvisor::Event;

    fn test_spec() -> TaskSpec {
        TaskSpec::builder(
            "slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .expect("valid spec")
    }

    fn changed_test_spec() -> TaskSpec {
        TaskSpec::builder(
            "slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v2").unwrap()),
            6_000_u64,
        )
        .build()
        .expect("valid spec")
    }

    fn add_test_task(state: &TaskState, task_name: &str) -> TaskId {
        let id = TaskId::new(task_name).unwrap();
        state.add_task(TaskManifest::new(id.clone(), test_spec()).expect("valid manifest"));
        id
    }

    fn bind_test_task(state: &TaskState, id: &TaskId, tv: taskvisor::TaskId) -> RuntimeBinding {
        let resource = ResourceGeneration::from_task(&state.get(id).expect("task must exist"));
        assert!(state.bind_tv(resource.clone(), tv));
        RuntimeBinding { resource, tv }
    }

    trait TestTaskStateExt {
        fn tv_for(&self, id: &TaskId) -> Option<taskvisor::TaskId>;
    }

    impl TestTaskStateExt for TaskState {
        fn tv_for(&self, id: &TaskId) -> Option<taskvisor::TaskId> {
            self.binding_for(id).map(|binding| binding.tv)
        }
    }

    fn setup(task_name: &str) -> (RuntimeObserver, TaskState, TaskId) {
        let state = TaskState::new();
        let id = add_test_task(&state, task_name);
        let binding = bind_test_task(&state, &id, taskvisor::TaskId::for_tests());
        assert!(state.transition_attempt_starting(&binding, 1));
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );
        (sub, state, id)
    }

    fn bound_event(state: &TaskState, id: &TaskId, kind: EventKind) -> Event {
        Event::new(kind).with_id(state.tv_for(id).expect("task must be bound"))
    }

    #[test]
    fn prepared_binding_routes_the_first_event_without_replay() {
        let state = TaskState::new();
        let id = add_test_task(&state, "prepared-start");
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));
        let tv = taskvisor::TaskId::for_tests();
        let resource = ResourceGeneration::from_task(&state.get(&id).expect("task must exist"));

        assert!(sub.bind(resource, tv, true));
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(tv)
                .with_attempt(1),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
        assert_eq!(state.list_runs(&id).len(), 1);
        assert_eq!(state.list_runs(&id)[0].attempt(), 1);
        assert!(registry.subscribe_raw(&id).is_some());
    }

    #[test]
    fn terminal_event_uses_authoritative_attempt_when_start_was_dropped() {
        let state = TaskState::new();
        let id = add_test_task(&state, "dropped-start");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let mut output = registry.subscribe_raw(&id).expect("output channel");
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_id(tv)
                .with_attempt(2)
                .with_reason("start event was dropped")
                .with_exit_code(7),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().attempt(), 2);
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 2));
        assert_eq!(runs[0].phase(), TaskPhase::Failed);
        assert!(matches!(
            output.try_recv(),
            Ok(OutputEvent::RunFinished {
                generation: 1,
                attempt: 2,
                exit_code: Some(7),
                ..
            })
        ));
    }

    #[test]
    fn attempt_event_without_attempt_is_ignored_without_attempt_zero() {
        let state = TaskState::new();
        let id = add_test_task(&state, "missing-attempt");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let mut output = registry.subscribe_raw(&id).expect("output channel");
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_id(tv)
                .with_reason("missing authoritative attempt"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Pending);
        assert_eq!(task.status().attempt(), 0);
        assert!(state.list_runs(&id).is_empty());
        assert!(output.try_recv().is_err());
    }

    #[test]
    fn old_generation_attempt_closes_only_its_run() {
        let state = TaskState::new();
        let id = add_test_task(&state, "old-generation");
        let tv = taskvisor::TaskId::for_tests();
        let old_binding = bind_test_task(&state, &id, tv);
        assert!(state.transition_attempt_starting(&old_binding, 1));
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let mut output = registry.subscribe_raw(&id).expect("output channel");
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        let desired =
            TaskManifest::new(id.clone(), changed_test_spec()).expect("valid desired task");
        let commit = state.apply_desired(&desired).expect("apply must succeed");
        assert_eq!(commit.task.metadata().generation(), 2);
        assert_eq!(commit.task.status().phase(), TaskPhase::Pending);

        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_id(tv)
                .with_attempt(1)
                .with_reason("old generation stopped")
                .with_exit_code(9),
        );

        let current = state.get(&id).expect("current task exists");
        assert_eq!(current.metadata().generation(), 2);
        assert_eq!(current.status().phase(), TaskPhase::Pending);
        assert_eq!(current.status().attempt(), 0);
        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 1));
        assert_eq!(runs[0].phase(), TaskPhase::Failed);
        assert!(matches!(
            output.try_recv(),
            Ok(OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(9),
                ..
            })
        ));
    }

    #[test]
    fn stale_incarnation_event_cannot_touch_recreated_resource() {
        let state = TaskState::new();
        let id = add_test_task(&state, "recreated");
        let old_task = state.get(&id).expect("old task exists");
        let old_tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, old_tv);
        assert!(state.delete_task(&id));

        let recreated_id = add_test_task(&state, "recreated");
        let recreated = state.get(&recreated_id).expect("new task exists");
        assert_ne!(old_task.uid(), recreated.uid());
        let new_tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &recreated_id, new_tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(recreated_id.clone());
        let mut output = registry
            .subscribe_raw(&recreated_id)
            .expect("output channel");
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_id(old_tv)
                .with_attempt(1)
                .with_reason("late old incarnation"),
        );

        let current = state.get(&recreated_id).expect("new task remains");
        assert_eq!(current.uid(), recreated.uid());
        assert_eq!(current.status().phase(), TaskPhase::Pending);
        assert!(state.list_runs(&recreated_id).is_empty());
        assert_eq!(state.tv_for(&recreated_id), Some(new_tv));
        assert!(output.try_recv().is_err());
    }

    #[test]
    fn failed_intake_cleanup_is_fenced_by_exact_binding() {
        let state = TaskState::new();
        let id = add_test_task(&state, "intake-failure");
        let old_tv = taskvisor::TaskId::for_tests();
        let stale = bind_test_task(&state, &id, old_tv);
        let current_tv = taskvisor::TaskId::for_tests();
        let current = bind_test_task(&state, &id, current_tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        assert!(!sub.fail_bound_reconciliation(
            &stale,
            "RuntimeSubmissionFailed",
            "stale intake failure".into(),
        ));
        assert_eq!(state.tv_for(&id), Some(current_tv));
        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Pending);
        assert!(registry.subscribe_raw(&id).is_some());

        assert!(sub.fail_bound_reconciliation(
            &current,
            "RuntimeSubmissionFailed",
            "controller intake failed".into(),
        ));
        assert!(state.tv_for(&id).is_none());
        let failed = state.get(&id).expect("desired resource is retained");
        assert_eq!(failed.status().phase(), TaskPhase::Pending);
        assert_eq!(failed.status().observed_generation(), 1);
        assert_eq!(failed.status().attempt(), 0);
        assert!(failed.status().error().is_none());
        assert_eq!(
            failed.status().reconciled().status(),
            ConditionStatus::False
        );
        assert_eq!(
            failed.status().reconciled().reason(),
            "RuntimeSubmissionFailed"
        );
        assert_eq!(
            failed.status().reconciled().message(),
            "controller intake failed"
        );
        assert!(registry.subscribe_raw(&id).is_none());
    }

    #[tokio::test]
    async fn task_removed_barrier_preserves_queued_attempt_events_before_cleanup() {
        let state = TaskState::new();
        let id = add_test_task(&state, "fast-attempt");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let mut output = registry.subscribe_raw(&id).expect("output channel");
        let sub = Arc::new(RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::clone(&registry),
        ));

        let completion = {
            let sub = Arc::clone(&sub);
            tokio::spawn(async move {
                sub.finalize_from_outcome(tv.get(), &taskvisor::TaskOutcome::Completed)
                    .await;
            })
        };
        tokio::task::yield_now().await;

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(tv)
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::AttemptSucceeded)
                .with_id(tv)
                .with_attempt(1),
        );
        sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));
        completion.await.expect("completion task");

        let task = state.get(&id).expect("retained terminal task");
        assert_eq!(task.status().phase(), TaskPhase::Succeeded);
        assert_eq!(state.list_runs(&id).len(), 1);
        assert_eq!(state.list_runs(&id)[0].phase(), TaskPhase::Succeeded);
        assert!(state.tv_for(&id).is_none());
        assert!(registry.subscribe_raw(&id).is_none());
        assert!(matches!(
            output.try_recv(),
            Ok(OutputEvent::RunStarted { attempt: 1, .. })
        ));
        assert!(matches!(
            output.try_recv(),
            Ok(OutputEvent::RunFinished { attempt: 1, .. })
        ));
    }

    #[test]
    fn late_events_after_completion_are_ignored() {
        let (sub, state, id) = setup("late-after-complete");
        let tv = state.tv_for(&id).expect("bound task");

        sub.finalize_outcome_immediately_for_test(tv.get(), &taskvisor::TaskOutcome::Completed);
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(tv)
                .with_attempt(2),
        );
        sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));

        assert!(state.tv_for(&id).is_none());
        assert_eq!(
            state.get(&id).unwrap().status().phase(),
            TaskPhase::Succeeded
        );
    }

    #[tokio::test]
    async fn late_outcome_after_explicit_delete_skips_the_barrier_wait() {
        let (sub, state, id) = setup("deleted-before-outcome");
        let tv = state.tv_for(&id).expect("bound task");

        assert!(sub.delete_after_cleanup(&id, Some(tv)));
        tokio::time::timeout(
            Duration::from_millis(100),
            sub.finalize_from_outcome(tv.get(), &taskvisor::TaskOutcome::Completed),
        )
        .await
        .expect("a completed identity must not create a new barrier");

        assert!(state.get(&id).is_none());
    }

    #[test]
    fn idempotent_delete_does_not_evict_an_unknown_external_channel() {
        let state = TaskState::new();
        let id = TaskId::new("not-yet-submitted").unwrap();
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let sub = RuntimeObserver::with_output_hub(state, Arc::clone(&registry));

        assert!(!sub.delete_after_cleanup(&id, None));
        assert!(registry.subscribe_raw(&id).is_some());
    }

    #[test]
    fn waiter_error_releases_binding_only_after_task_removed_barrier() {
        let (sub, state, id) = setup("missing-outcome");
        let tv = state.tv_for(&id).expect("bound task");

        sub.finalize_unavailable(tv.get(), "task outcome unavailable: shutting down".into());
        assert!(
            state.tv_for(&id).is_some(),
            "channel closure alone must fail closed while task cleanup is unproven"
        );

        sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));

        assert!(state.tv_for(&id).is_none());
        let task = state.get(&id).expect("retained failed task");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert!(
            task.status()
                .error()
                .is_some_and(|error| error.contains("outcome unavailable"))
        );
    }

    #[test]
    fn runtime_failures_are_diagnostic_only() {
        for (name, reason) in [
            ("remove-diagnostic", "remove_failed: registry closed"),
            ("future-diagnostic", "future_controller_diagnostic: detail"),
        ] {
            let (sub, state, id) = setup(name);
            let tv = state.tv_for(&id).expect("bound task");

            sub.on_event(
                &Event::new(EventKind::RuntimeFailure)
                    .with_id(tv)
                    .with_reason(reason),
            );

            assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
            assert_eq!(state.tv_for(&id), Some(tv));
        }
    }

    #[test]
    fn typed_controller_rejections_project_state() {
        for (name, kind, reason, expected) in [
            (
                "drop-rejection",
                taskvisor::RejectionKind::SlotBusy,
                "slot is busy; this diagnostic text is not schema",
                TaskPhase::Canceled,
            ),
            (
                "add-rejection",
                taskvisor::RejectionKind::AdmissionFailed,
                "add_failed: command queue closed",
                TaskPhase::Failed,
            ),
            (
                "queue-start-rejection",
                taskvisor::RejectionKind::AdmissionFailed,
                "queue_start_failed: shutting down",
                TaskPhase::Failed,
            ),
        ] {
            let (sub, state, id) = setup(name);
            let tv = state.tv_for(&id).expect("bound task");

            sub.on_event(
                &Event::new(EventKind::ControllerRejected)
                    .with_id(tv)
                    .with_rejection_kind(kind)
                    .with_reason(reason),
            );

            assert_eq!(state.get(&id).unwrap().status().phase(), expected);
            assert_eq!(state.tv_for(&id), Some(tv));
        }
    }

    #[test]
    fn task_add_failed_is_always_terminal_for_its_identity() {
        let (sub, state, id) = setup("registry-add-failed");
        let tv = state.tv_for(&id).expect("bound task");

        sub.on_event(
            &Event::new(EventKind::TaskAddFailed)
                .with_id(tv)
                .with_rejection_kind(taskvisor::RejectionKind::AdmissionFailed)
                .with_reason("future_registry_rejection"),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Failed);
        assert_eq!(state.tv_for(&id), Some(tv));
    }

    #[test]
    fn task_removed_is_observability_only_for_current_and_stale_identities() {
        let (sub, state, id) = setup("reuse-x");
        let tvs = [
            taskvisor::TaskId::for_tests(),
            taskvisor::TaskId::for_tests(),
        ];
        bind_test_task(&state, &id, tvs[0]);
        bind_test_task(&state, &id, tvs[1]);

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
            state.get(&id).is_some(),
            "TaskRemoved must not bypass terminal-state retention"
        );
        assert_eq!(
            state.tv_for(&id).map(|tv| tv.get()),
            Some(tvs[1].get()),
            "the direct completion path remains the binding owner"
        );
    }

    #[test]
    fn controller_rejection_projects_phase_but_waiter_owns_cleanup() {
        let state = TaskState::new();
        let id = add_test_task(&state, "rejected-task");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("some-slot")
            .with_id(tv)
            .with_rejection_kind(taskvisor::RejectionKind::QueueFull)
            .with_reason("queue_full: 3/3");
        sub.on_event(&ev);

        let task = state.get(&id).expect("entry kept for observability");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert!(
            task.status()
                .error()
                .is_some_and(|e| e.contains("queue_full")),
            "rejection reason must be recorded"
        );
        assert!(
            registry.subscribe_raw(&id).is_some(),
            "the event path must not race the waiter's output cleanup"
        );
        assert!(
            state.tv_for(&id).is_some(),
            "the binding stays owned until direct completion resolves"
        );
    }

    #[test]
    fn controller_rejection_becomes_sweepable_only_after_waiter_cleanup() {
        use crate::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = add_test_task(&state, "rej-reap");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );

        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("slot")
                .with_id(tv)
                .with_rejection_kind(taskvisor::RejectionKind::QueueFull)
                .with_reason("queue_full: 3/3"),
        );

        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 0, "a still-bound submission must not be swept");

        assert_eq!(
            state.finalize_if_bound(
                tv.get(),
                TaskPhase::Failed,
                Some("queue_full: 3/3".into()),
                None,
                true,
            ),
            Some(id.clone()),
        );
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "waiter cleanup makes the entry sweepable");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn task_finished_failed_becomes_sweepable_only_after_waiter_cleanup() {
        use crate::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = add_test_task(&state, "exh-reap");
        let tv = taskvisor::TaskId::for_tests();
        let binding = bind_test_task(&state, &id, tv);
        assert!(state.transition_attempt_starting(&binding, 1));
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("exh-reap")
                .with_id(tv)
                .with_outcome_kind(TaskOutcomeKind::Failed)
                .with_reason("retry policy stopped after one retry"),
        );
        assert!(
            state.tv_for(&id).is_some(),
            "TaskFinished projects state but the direct outcome still owns the binding"
        );

        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 0, "a still-bound task must not be reaped");

        assert_eq!(
            state.finalize_if_bound(tv.get(), TaskPhase::Exhausted, None, None, false),
            Some(id.clone()),
        );
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "waiter cleanup makes the entry sweepable");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn task_finished_fatal_becomes_sweepable_only_after_waiter_cleanup() {
        use crate::StateConfig;
        use std::time::Duration;

        let state = TaskState::new();
        let id = add_test_task(&state, "dead-reap");
        let tv = taskvisor::TaskId::for_tests();
        let binding = bind_test_task(&state, &id, tv);
        assert!(state.transition_attempt_starting(&binding, 1));
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("dead-reap")
                .with_id(tv)
                .with_outcome_kind(TaskOutcomeKind::Fatal)
                .with_reason("fatal error (no retry): boom"),
        );
        assert!(
            state.tv_for(&id).is_some(),
            "TaskFinished must not take binding ownership from the direct outcome"
        );

        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 0, "a still-bound task must not be reaped");

        assert_eq!(
            state.finalize_if_bound(tv.get(), TaskPhase::Failed, None, None, false),
            Some(id.clone()),
        );
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "waiter cleanup makes the entry sweepable");
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn user_initiated_queue_removal_is_canceled_phase() {
        let state = TaskState::new();
        let id = add_test_task(&state, "victim");
        let tv = taskvisor::TaskId::for_tests();
        bind_test_task(&state, &id, tv);
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );

        let ev = Event::new(EventKind::ControllerRejected)
            .with_task("s")
            .with_id(tv)
            .with_rejection_kind(taskvisor::RejectionKind::RemovedFromQueue)
            .with_reason("removed_from_queue");
        sub.on_event(&ev);

        let task = state.get(&id).expect("entry kept");
        assert_eq!(
            task.status().phase(),
            TaskPhase::Canceled,
            "user-initiated removal is a cancellation, not a failure"
        );
    }

    #[test]
    fn attempt_canceled_maps_to_canceled_phase() {
        let (sub, state, id) = setup("graceful");

        let ev = bound_event(&state, &id, EventKind::AttemptCanceled).with_attempt(1);
        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Canceled);
    }

    #[test]
    fn task_finished_fatal_seals_phase_as_failed_with_exit_code() {
        let (sub, state, id) = setup("fatal-task");

        let ev = bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Fatal)
            .with_reason("fatal error (no retry): boom")
            .with_exit_code(137);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert_eq!(task.status().exit_code(), Some(137));
        assert_eq!(task.status().error(), Some("fatal error (no retry): boom"),);
        assert!(task.status().phase().is_terminal());
    }

    #[test]
    fn task_finished_fatal_with_no_exit_code_stores_none() {
        let (sub, state, id) = setup("logical-fatal");

        let ev = bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Fatal)
            .with_reason("fatal error (no retry): misconfigured");

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert_eq!(task.status().exit_code(), None);
    }

    #[test]
    fn runtime_failure_without_identity_does_not_touch_user_task() {
        let (sub, state, id) = setup("controller");

        let ev = Event::new(EventKind::RuntimeFailure)
            .with_task("controller")
            .with_reason("controller_loop_exited: boom");

        sub.on_event(&ev);

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
    }

    #[test]
    fn attempt_failed_carries_event_exit_code_into_state() {
        let (sub, state, id) = setup("fail-task");

        let ev = bound_event(&state, &id, EventKind::AttemptFailed)
            .with_attempt(1)
            .with_reason("execution failed: non-zero")
            .with_exit_code(2);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert_eq!(task.status().exit_code(), Some(2));
    }

    #[test]
    fn task_finished_failed_carries_event_exit_code_into_state() {
        let (sub, state, id) = setup("exhausted");

        let ev = bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Failed)
            .with_reason("retry limit reached after five retries")
            .with_exit_code(1);

        sub.on_event(&ev);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Exhausted);
        assert_eq!(task.status().exit_code(), Some(1));
    }

    fn setup_pending_with_output_hub(
        task_name: &str,
    ) -> (RuntimeObserver, TaskState, Arc<OutputHub>, TaskId) {
        let state = TaskState::new();
        let id = add_test_task(&state, task_name);
        bind_test_task(&state, &id, taskvisor::TaskId::for_tests());
        let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
        registry.ensure_channel(id.clone());
        let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));
        (sub, state, registry, id)
    }

    fn setup_with_output_hub(
        task_name: &str,
    ) -> (RuntimeObserver, TaskState, Arc<OutputHub>, TaskId) {
        let setup = setup_pending_with_output_hub(task_name);
        let binding = setup.1.binding_for(&setup.3).expect("task must be bound");
        assert!(setup.1.transition_attempt_starting(&binding, 1));
        setup
    }

    #[test]
    fn attempt_starting_announces_run_started_into_output_hub() {
        let (sub, state, registry, id) = setup_pending_with_output_hub("started-1");
        let mut rx = registry.subscribe_raw(&id).unwrap();

        let ev = bound_event(&state, &id, EventKind::AttemptStarting).with_attempt(1);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunStarted {
                generation,
                attempt,
                ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(attempt, 1);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[test]
    fn attempt_succeeded_announces_run_finished_with_no_exit_code() {
        let (sub, state, registry, id) = setup_with_output_hub("stopped-1");
        let mut rx = registry.subscribe_raw(&id).unwrap();

        let ev = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(attempt, 1);
                assert_eq!(exit_code, None);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_attempt_succeeded_does_not_announce_a_second_run_finished() {
        let (sub, state, registry, id) = setup_with_output_hub("stopped-duplicate");
        let mut rx = registry.subscribe_raw(&id).unwrap();
        let event = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);

        sub.on_event(&event);
        sub.on_event(&event);

        assert!(matches!(
            rx.try_recv(),
            Ok(OutputEvent::RunFinished { attempt: 1, .. })
        ));
        assert!(
            rx.try_recv().is_err(),
            "a duplicate terminal attempt event must be an exact no-op"
        );
    }

    #[test]
    fn duplicate_old_generation_terminal_does_not_announce_again_after_apply() {
        let (sub, state, registry, id) = setup_with_output_hub("stopped-old-generation");
        let mut rx = registry.subscribe_raw(&id).unwrap();
        let event = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);

        sub.on_event(&event);
        assert!(matches!(
            rx.try_recv(),
            Ok(OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                ..
            })
        ));

        state
            .apply_desired(&TaskManifest::new(id.clone(), changed_test_spec()).unwrap())
            .unwrap();
        sub.on_event(&event);

        assert!(
            rx.try_recv().is_err(),
            "a duplicate terminal event from the previous generation must remain a no-op"
        );
    }

    #[test]
    fn attempt_failed_announces_run_finished_with_exit_code() {
        let (sub, state, registry, id) = setup_with_output_hub("failed-1");
        let mut rx = registry.subscribe_raw(&id).unwrap();

        let ev = bound_event(&state, &id, EventKind::AttemptFailed)
            .with_attempt(1)
            .with_exit_code(17);
        sub.on_event(&ev);

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(attempt, 1);
                assert_eq!(exit_code, Some(17));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn task_finished_refines_state_without_duplicate_run_finished() {
        let (sub, state, registry, id) = setup_with_output_hub("exh-evict");
        let mut rx = registry.subscribe_raw(&id).unwrap();

        sub.on_event(
            &bound_event(&state, &id, EventKind::AttemptFailed)
                .with_attempt(1)
                .with_exit_code(1),
        );
        sub.on_event(
            &bound_event(&state, &id, EventKind::TaskFinished)
                .with_outcome_kind(TaskOutcomeKind::Failed)
                .with_reason("retry policy stopped")
                .with_exit_code(1),
        );

        match rx.try_recv().unwrap() {
            OutputEvent::RunFinished {
                generation,
                attempt,
                ..
            } => {
                assert_eq!(generation, 1);
                assert_eq!(attempt, 1);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "TaskFinished is task-level and must not announce a second RunFinished"
        );
        assert!(
            registry.subscribe_raw(&id).is_some(),
            "the direct completion path owns terminal channel eviction"
        );
    }

    #[test]
    fn attempt_timed_out_is_a_single_terminal_attempt_event() {
        let (sub, state, registry, id) = setup_with_output_hub("slow-task");
        let mut rx = registry.subscribe_raw(&id).unwrap();

        sub.on_event(
            &bound_event(&state, &id, EventKind::AttemptTimedOut)
                .with_attempt(1)
                .with_timeout(Duration::from_millis(250)),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Timeout);
        assert_eq!(
            task.status().error(),
            Some("task attempt timed out after 250 ms"),
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(OutputEvent::RunFinished { attempt: 1, .. })
        ));
    }

    #[test]
    fn attempt_failed_stays_failed() {
        let (sub, state, id) = setup("plain-fail");

        sub.on_event(
            &bound_event(&state, &id, EventKind::AttemptFailed)
                .with_attempt(1)
                .with_reason("boom"),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Failed);
    }

    #[test]
    fn task_finished_canceled_maps_by_kind_not_reason() {
        let (sub, state, id) = setup("self-cancel");

        sub.on_event(
            &bound_event(&state, &id, EventKind::TaskFinished)
                .with_outcome_kind(TaskOutcomeKind::Canceled)
                .with_reason("text that must not select a phase"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(
            task.status().phase(),
            TaskPhase::Canceled,
            "the typed outcome selects cancellation"
        );
        assert!(task.status().error().is_none());
    }

    #[test]
    fn task_finished_runtime_failures_use_typed_outcomes() {
        for (name, kind, expected_phase, expected_error) in [
            (
                "force-aborted",
                TaskOutcomeKind::ForceAborted,
                TaskPhase::Canceled,
                crate::map::phase::FORCE_ABORTED_ERROR,
            ),
            (
                "runner-panicked",
                TaskOutcomeKind::Panicked,
                TaskPhase::Failed,
                crate::map::phase::TASK_RUNNER_PANICKED_ERROR,
            ),
        ] {
            let (sub, state, id) = setup(name);

            sub.on_event(
                &bound_event(&state, &id, EventKind::TaskFinished)
                    .with_outcome_kind(kind)
                    .with_reason("diagnostic text that must not select the phase"),
            );

            let task = state.get(&id).expect("task exists");
            assert_eq!(task.status().phase(), expected_phase);
            assert_eq!(task.status().error(), Some(expected_error));
        }
    }

    #[test]
    fn task_finished_completed_after_success_is_not_an_error() {
        let (sub, state, id) = setup("oneshot");

        sub.on_event(&bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1));
        sub.on_event(
            &bound_event(&state, &id, EventKind::TaskFinished)
                .with_outcome_kind(TaskOutcomeKind::Completed)
                .with_reason("diagnostic text that looks like a failure"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(
            task.status().phase(),
            TaskPhase::Succeeded,
            "normal one-shot completion must stay Succeeded"
        );
        assert!(
            task.status().error().is_none(),
            "Completed ignores diagnostic reason text"
        );
    }

    #[test]
    fn task_finished_without_outcome_kind_does_not_guess_from_reason() {
        let (sub, state, id) = setup("missing-kind");

        sub.on_event(
            &bound_event(&state, &id, EventKind::TaskFinished)
                .with_reason("fatal-looking diagnostic"),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
    }

    #[test]
    fn task_finished_fatal_waits_for_waiter_to_evict_channel() {
        let (sub, state, registry, id) = setup_with_output_hub("dead-evict");

        let ev = bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Fatal)
            .with_exit_code(137);
        sub.on_event(&ev);

        assert!(
            registry.subscribe_raw(&id).is_some(),
            "the direct completion path owns terminal channel eviction"
        );
    }

    #[test]
    fn task_removed_does_not_bypass_waiter_cleanup_or_retention() {
        let (sub, state, registry, id) = setup_with_output_hub("remove");

        let ev = bound_event(&state, &id, EventKind::TaskRemoved);
        sub.on_event(&ev);

        assert!(
            registry.subscribe_raw(&id).is_some(),
            "TaskRemoved must not race the waiter's output cleanup"
        );
        assert!(state.get(&id).is_some(), "task_ttl retention stays intact");
        assert!(state.tv_for(&id).is_some(), "waiter still owns the binding");
    }
}
