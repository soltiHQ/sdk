//! # Runtime observer
//!
//! [`RuntimeObserver`] projects Taskvisor lifecycle events into SDK state.
//!
//! ## Flow
//!
//! ```text
//! Taskvisor event queue
//!       ▼
//! RuntimeObserver
//!       ├── attempt events ──► Task status and TaskRun
//!       ├── TaskFinished ────► resource phase
//!       └── attempt events ──► RunStarted and RunFinished output markers
//!
//! direct TaskOutcome
//!       └──► authoritative finalization and cleanup
//! ```
//!
//! Event delivery is best-effort.
//! Attempt detail can be lost.
//! The direct outcome still finalizes the resource.
//!
//! `TaskRemoved` normally acts as a FIFO barrier.
//! It lets queued attempt events arrive before binding and output cleanup.
//! Finalization waits for that barrier for at most one second.
//! Subscriber overflow releases finalizations that are safe without the barrier.

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

/// Serializes short lifecycle commits.
///
/// Event, completion, and management paths share this gate.
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

/// Taskvisor subscriber that updates SDK state.
///
/// It also publishes run boundary markers to the output hub.
///
/// ## See Also
///
/// - [`TaskState`] stores the projected state.
/// - [`SupervisorApiBuilder`](crate::SupervisorApiBuilder) installs this observer.
pub(crate) struct RuntimeObserver {
    state: TaskState,
    output_hub: Arc<OutputHub>,
    lifecycle_gate: LifecycleGate,
    completion_barriers: Mutex<CompletionBarriers>,
}

impl RuntimeObserver {
    /// Creates an observer over shared state and output.
    pub(crate) fn with_output_hub(state: TaskState, output_hub: Arc<OutputHub>) -> Self {
        Self {
            state,
            output_hub,
            lifecycle_gate: LifecycleGate::default(),
            completion_barriers: Mutex::new(CompletionBarriers::default()),
        }
    }

    /// Binds a prepared submission before it can publish events.
    ///
    /// Returns `false` for a stale UID or generation.
    pub(crate) fn bind(
        &self,
        resource: ResourceGeneration,
        tv: taskvisor::TaskId,
        ensure_output: bool,
    ) -> bool {
        let _lifecycle = self.lifecycle_gate.lock();
        let task_id = resource.name.clone();
        let task_uid = resource.uid.clone();
        if !self.state.bind_tv(resource, tv) {
            return false;
        }
        if ensure_output {
            self.output_hub.ensure_channel_if_absent(task_id, task_uid);
        }
        true
    }

    /// Releases a binding whose prepared submission was cancelled before intake.
    ///
    /// No Taskvisor event can exist before controller intake. The desired state
    /// remains pending for the newer reconciliation instead of recording an
    /// intake failure for the superseded generation.
    pub(crate) fn release_unsubmitted_binding(&self, binding: &RuntimeBinding) -> bool {
        let _lifecycle = self.lifecycle_gate.lock();
        if self.state.resolve_tv(binding.tv.get()).as_ref() != Some(binding) {
            return false;
        }

        self.state.unbind_tv(binding.tv.get());
        self.output_hub.evict(&binding.resource.name);
        self.mark_completed_locked(binding.tv.get());
        true
    }

    /// Releases a failed runtime intake binding.
    ///
    /// The desired resource remains with `Reconciled=False`.
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

    /// Finalizes a binding from the direct completion channel.
    ///
    /// Registered tasks wait briefly for the `TaskRemoved` barrier.
    /// Rejected tasks do not enter the registry and skip that wait.
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

    /// Finalizes an unavailable outcome after confirmed cleanup.
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

    /// Preserves a waiter failure until `TaskRemoved` proves cleanup.
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

    /// Waits for finalization after Taskvisor confirms cleanup.
    ///
    /// The completion task owns the authoritative result.
    /// Management waits instead of inventing a replacement result.
    /// This keeps cancellation and same-name resubmission ordered.
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

    /// Deletes local state after Taskvisor cleanup is settled.
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

    /// Releases every deferred outcome after Taskvisor has confirmed global shutdown.
    ///
    /// At that point no registered runtime remains. A missing per-task
    /// `TaskRemoved` event is no longer needed as cleanup evidence.
    pub(crate) fn finalize_pending_after_confirmed_shutdown(&self) {
        let _lifecycle = self.lifecycle_gate.lock();
        let pending = {
            let mut barriers = self.completion_barriers.lock();
            barriers.pending.drain().collect::<Vec<_>>()
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

    /// Resolves the binding for an event.
    fn resolve(&self, event: &Event) -> Option<RuntimeBinding> {
        self.state.resolve_tv(event.id?.get())
    }
}

impl RuntimeObserver {
    /// Applies one event while holding the lifecycle gate.
    fn apply_event_locked(&self, event: &Event) {
        let Some(tv) = event.id else {
            return;
        };
        let tv_raw = tv.get();

        let Some(binding) = self.resolve(event) else {
            return;
        };
        let task_id = &binding.resource.name;
        let task_uid = &binding.resource.uid;
        let generation = binding.resource.generation;

        // Output cleanup does not own the retained SDK resource. The
        // direct completion finalizes it; explicit delete removes it eagerly.
        if event.kind == EventKind::TaskRemoved {
            self.task_removed_locked(tv_raw);
            return;
        }

        match event.kind {
            EventKind::TaskAdded => {
                trace!(
                    event = "taskvisor.event",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    event_kind = "task_added",
                    "task event received"
                );
            }
            EventKind::AttemptStarting => {
                let Some(attempt) = event.attempt else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "attempt_starting",
                        "task event missing attempt"
                    );
                    return;
                };
                trace!(
                    event = "task.attempt",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    attempt,
                    stage = "starting",
                    "task attempt starting"
                );
                if self.state.transition_attempt_starting(&binding, attempt) {
                    self.output_hub.announce_run_started(
                        task_id,
                        &binding.resource.uid,
                        binding.resource.generation,
                        attempt,
                    );
                } else {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        event_kind = "attempt_starting",
                        "stale task event ignored"
                    );
                }
            }
            EventKind::AttemptSucceeded => {
                let Some(attempt) = event.attempt else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "attempt_succeeded",
                        "task event missing attempt"
                    );
                    return;
                };
                trace!(
                    event = "task.attempt",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    attempt,
                    stage = "succeeded",
                    "task attempt succeeded"
                );
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Succeeded,
                    None,
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        &binding.resource.uid,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        event_kind = "attempt_succeeded",
                        "stale task event ignored"
                    );
                }
            }
            EventKind::AttemptCanceled => {
                let Some(attempt) = event.attempt else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "attempt_canceled",
                        "task event missing attempt"
                    );
                    return;
                };
                trace!(
                    event = "task.attempt",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    attempt,
                    stage = "canceled",
                    "task attempt canceled"
                );
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Canceled,
                    None,
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        &binding.resource.uid,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        event_kind = "attempt_canceled",
                        "stale task event ignored"
                    );
                }
            }
            EventKind::AttemptFailed => {
                let Some(attempt) = event.attempt else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "attempt_failed",
                        "task event missing attempt"
                    );
                    return;
                };
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                trace!(
                    event = "task.attempt",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    attempt,
                    stage = "failed",
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
                        &binding.resource.uid,
                        binding.resource.generation,
                        attempt,
                        event.exit_code,
                    );
                } else {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        event_kind = "attempt_failed",
                        "stale task event ignored"
                    );
                }
            }
            EventKind::AttemptTimedOut => {
                let Some(attempt) = event.attempt else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "attempt_timed_out",
                        "task event missing attempt"
                    );
                    return;
                };
                let error = event.timeout_ms.map_or_else(
                    || "task attempt timed out".to_string(),
                    |timeout_ms| format!("task attempt timed out after {timeout_ms} ms"),
                );
                trace!(
                    event = "task.attempt",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    attempt,
                    stage = "timed_out",
                    "task attempt timed out"
                );
                if self.state.transition_attempt_finished(
                    &binding,
                    attempt,
                    TaskPhase::Timeout,
                    Some(error),
                    None,
                ) {
                    self.output_hub.announce_run_finished(
                        task_id,
                        &binding.resource.uid,
                        binding.resource.generation,
                        attempt,
                        None,
                    );
                } else {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        event_kind = "attempt_timed_out",
                        "stale task event ignored"
                    );
                }
            }
            EventKind::TaskFinished => {
                let Some(outcome_kind) = event.outcome_kind else {
                    warn!(
                        event = "taskvisor.event_invalid",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "task_finished",
                        "task event missing outcome"
                    );
                    return;
                };
                let (phase, error, exit_code) =
                    phase_for_outcome_kind(outcome_kind, event.reason.as_deref(), event.exit_code);
                trace!(
                    event = "task.finished",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    outcome = outcome_kind.as_label(),
                    exit_code = ?exit_code,
                    "task reached final outcome",
                );
                if !self
                    .state
                    .transition_task_finished(&binding, phase, error, exit_code)
                {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "task_finished",
                        "stale task event ignored"
                    );
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
                trace!(
                    event = "task.finished",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    outcome = "controller_rejected",
                    "task rejected"
                );
                if !self
                    .state
                    .transition_task_finished(&binding, phase, Some(reason), None)
                {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "controller_rejected",
                        "stale task event ignored"
                    );
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
                trace!(
                    event = "task.finished",
                    task_name = %task_id,
                    task_uid = %task_uid,
                    generation,
                    taskvisor_id = tv_raw,
                    outcome = "task_add_failed",
                    "task submission failed"
                );
                if !self
                    .state
                    .transition_task_finished(&binding, phase, Some(reason), None)
                {
                    warn!(
                        event = "taskvisor.event_stale",
                        task_name = %task_id,
                        task_uid = %task_uid,
                        generation,
                        taskvisor_id = tv_raw,
                        event_kind = "task_add_failed",
                        "stale task event ignored"
                    );
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

    /// Returns the observer queue capacity.
    ///
    /// The capacity is `2048`.
    /// This is twice Taskvisor's default subscriber capacity.
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
mod tests;
