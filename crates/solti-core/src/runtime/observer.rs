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
//! If state persistence admission is closed after authoritative cleanup, the
//! observer removes exact runtime ownership without synthesizing status or runs.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use taskvisor::{Event, EventKind, Subscribe};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, watch};
use tracing::{trace, warn};

use crate::map::phase::{phase_for_outcome, phase_for_outcome_kind, phase_for_rejection};
use crate::output::OutputHub;
use crate::persistence::{StateAdmissionClosed, block_on_thread};
use crate::state::{
    ResourceGeneration, RuntimeBinding, StateMutationEventCapacity, StateWriteAdmission, TaskState,
};
use solti_model::{TaskId, TaskPhase};

const RUNTIME_OBSERVER_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2048).unwrap();
const REMOVED_ID_CAPACITY: usize = 4096;
const EVENT_BARRIER_TIMEOUT: Duration = Duration::from_secs(1);

/// Serializes short lifecycle commits.
///
/// Event, completion, and management paths share this gate.
#[derive(Clone, Default)]
struct LifecycleGate {
    inner: Arc<AsyncMutex<()>>,
    #[cfg(test)]
    waiters: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
struct LifecycleWaiter {
    waiters: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl Drop for LifecycleWaiter {
    fn drop(&mut self) {
        self.waiters
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl LifecycleGate {
    #[cfg(test)]
    fn track_waiter(&self) -> LifecycleWaiter {
        self.waiters
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        LifecycleWaiter {
            waiters: Arc::clone(&self.waiters),
        }
    }

    async fn lock(&self) -> OwnedMutexGuard<()> {
        #[cfg(test)]
        let waiter = self.track_waiter();
        let guard = Arc::clone(&self.inner).lock_owned().await;
        #[cfg(test)]
        drop(waiter);
        guard
    }

    fn lock_from_taskvisor_callback(&self) -> OwnedMutexGuard<()> {
        #[cfg(test)]
        let waiter = self.track_waiter();
        let guard = block_on_thread(Arc::clone(&self.inner).lock_owned());
        #[cfg(test)]
        drop(waiter);
        guard
    }

    #[cfg(test)]
    fn waiters(&self) -> usize {
        self.waiters.load(std::sync::atomic::Ordering::Acquire)
    }
}

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
    completion_barriers: Arc<Mutex<CompletionBarriers>>,
}

/// Owns an exact runtime binding until Taskvisor intake is accepted.
///
/// The synchronous fallback is intentionally limited to a pre-intake binding.
/// Taskvisor cannot publish an event before successful controller intake, and
/// the reconciliation worker still owns the per-name runtime lock. This lets an
/// unexpected unwind remove the exact binding without awaiting the lifecycle
/// gate or inventing a Taskvisor outcome.
pub(crate) struct ProvisionalBinding {
    state: TaskState,
    output_hub: Arc<OutputHub>,
    completion_barriers: Arc<Mutex<CompletionBarriers>>,
    binding: Option<RuntimeBinding>,
}

impl ProvisionalBinding {
    fn new(
        state: TaskState,
        output_hub: Arc<OutputHub>,
        completion_barriers: Arc<Mutex<CompletionBarriers>>,
        binding: RuntimeBinding,
    ) -> Self {
        Self {
            state,
            output_hub,
            completion_barriers,
            binding: Some(binding),
        }
    }

    pub(crate) fn binding(&self) -> &RuntimeBinding {
        self.binding
            .as_ref()
            .expect("provisional binding is armed until release or Taskvisor handoff")
    }

    /// Releases the binding through the ordinary serialized observer path.
    pub(crate) async fn release(mut self, observer: &RuntimeObserver) -> bool {
        let binding = self.binding().clone();
        let released = observer.release_unsubmitted_binding(&binding).await;
        if observer.state.resolve_tv(binding.tv.get()).as_ref() != Some(&binding) {
            self.binding = None;
        }
        released
    }

    /// Transfers ownership to the authoritative Taskvisor completion worker.
    pub(crate) fn disarm(mut self) {
        self.binding = None;
    }

    fn release_exact_without_await(&mut self) {
        let Some(binding) = self.binding.take() else {
            return;
        };
        if self.state.resolve_tv(binding.tv.get()).as_ref() != Some(&binding) {
            return;
        }
        if self.state.unbind_exact(&binding) {
            self.output_hub
                .evict_if_uid(&binding.resource.name, &binding.resource.uid);
            let mut barriers = self.completion_barriers.lock();
            barriers.pending.remove(&binding.tv.get());
            barriers.take_removed(binding.tv.get());
            barriers.notify_finalized(binding.tv.get());
        }
    }
}

impl Drop for ProvisionalBinding {
    fn drop(&mut self) {
        let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            self.release_exact_without_await();
        })) else {
            return;
        };
        if let Err(nested) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            // A hostile panic payload cannot be destroyed recursively. Retain
            // this one nested payload and preserve the non-unwinding boundary.
            std::mem::forget(nested);
        }
    }
}

impl RuntimeObserver {
    /// Creates an observer over shared state and output.
    pub(crate) fn with_output_hub(state: TaskState, output_hub: Arc<OutputHub>) -> Self {
        Self {
            state,
            output_hub,
            lifecycle_gate: LifecycleGate::default(),
            completion_barriers: Arc::new(Mutex::new(CompletionBarriers::default())),
        }
    }

    /// Binds a prepared submission before it can publish events.
    ///
    /// Returns `None` for a stale UID or generation.
    pub(crate) async fn bind(
        &self,
        resource: ResourceGeneration,
        tv: taskvisor::TaskId,
        ensure_output: bool,
    ) -> Option<ProvisionalBinding> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        let binding = RuntimeBinding {
            resource: resource.clone(),
            tv,
        };
        if !self.state.bind_tv(resource, tv) {
            return None;
        }
        let provisional = ProvisionalBinding::new(
            self.state.clone(),
            Arc::clone(&self.output_hub),
            Arc::clone(&self.completion_barriers),
            binding.clone(),
        );
        if ensure_output {
            self.output_hub.ensure_channel_if_absent(
                binding.resource.name.clone(),
                binding.resource.uid.clone(),
            );
        }
        Some(provisional)
    }

    /// Releases a binding whose prepared submission was cancelled before intake.
    ///
    /// No Taskvisor event can exist before controller intake. The desired state
    /// remains pending for the newer reconciliation instead of recording an
    /// intake failure for the superseded generation.
    pub(crate) async fn release_unsubmitted_binding(&self, binding: &RuntimeBinding) -> bool {
        let _lifecycle = self.lifecycle_gate.lock().await;
        if !self.state.unbind_exact(binding) {
            return false;
        }
        self.output_hub
            .evict_if_uid(&binding.resource.name, &binding.resource.uid);
        self.mark_completed_locked(binding.tv.get());
        true
    }

    /// Releases a failed runtime intake binding.
    ///
    /// The desired resource remains with `Reconciled=False`.
    #[cfg(test)]
    pub(crate) async fn fail_bound_reconciliation(
        &self,
        binding: &RuntimeBinding,
        reason: &'static str,
        message: String,
    ) -> bool {
        let admission = self
            .state
            .admit_state_write(StateMutationEventCapacity::TaskChange)
            .await;
        let Ok(admission) = admission else {
            return false;
        };
        let _lifecycle = self.lifecycle_gate.lock().await;
        if !self.state.unbind_exact(binding) {
            return false;
        }
        self.output_hub
            .evict_if_uid(&binding.resource.name, &binding.resource.uid);
        let changed = self.state.mark_reconciliation_failed_admitted(
            &binding.resource,
            reason,
            message,
            admission,
        );
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
        let (ready, notification) = {
            let _lifecycle = self.lifecycle_gate.lock().await;
            self.register_finalization_locked(tv_raw, finalization, wait_for_barrier)
        };

        if let Some(finalization) = ready {
            self.finalize_async(tv_raw, finalization).await;
            return;
        }

        if let Some(notification) = notification
            && !Self::wait_for_finalization(notification).await
        {
            let pending = {
                let _lifecycle = self.lifecycle_gate.lock().await;
                self.take_safe_pending_locked(tv_raw)
            };
            if let Some(finalization) = pending {
                self.finalize_async(tv_raw, finalization).await;
            }
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
    pub(crate) async fn finalize_unavailable(&self, tv_raw: u64, error: String) {
        let finalization = Finalization {
            phase: TaskPhase::Failed,
            error: Some(error),
            exit_code: None,
            force: false,
            safe_without_barrier: false,
        };
        let ready = {
            let _lifecycle = self.lifecycle_gate.lock().await;
            self.register_finalization_locked(tv_raw, finalization, true)
                .0
        };
        if let Some(finalization) = ready {
            self.finalize_async(tv_raw, finalization).await;
        }
    }

    /// Waits for finalization after Taskvisor confirms cleanup.
    ///
    /// The completion task owns the authoritative result.
    /// Management waits instead of inventing a replacement result.
    /// This keeps cancellation and same-name resubmission ordered.
    pub(crate) async fn settle_after_confirmed_cleanup(&self, tv: taskvisor::TaskId) {
        let tv_raw = tv.get();
        let mut notification = {
            let _lifecycle = self.lifecycle_gate.lock().await;
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
    pub(crate) async fn delete_after_cleanup(
        &self,
        id: &TaskId,
        tv: Option<taskvisor::TaskId>,
    ) -> Result<bool, StateAdmissionClosed> {
        let admission = self
            .state
            .admit_state_write(StateMutationEventCapacity::TaskChange)
            .await?;
        let _lifecycle = self.lifecycle_gate.lock().await;
        let removed = self.state.delete_task_admitted(id, admission);
        if removed || tv.is_some() {
            self.output_hub.evict(id);
        }
        if let Some(tv) = tv {
            self.mark_completed_locked(tv.get());
        }
        Ok(removed)
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
    ) -> (Option<Finalization>, Option<watch::Receiver<bool>>) {
        if self.state.resolve_tv(tv_raw).is_none() {
            return (None, None);
        }

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
        (finalize_now, notification)
    }

    fn take_safe_pending_locked(&self, tv_raw: u64) -> Option<Finalization> {
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
    }

    fn force_all_safe_pending_from_taskvisor_callback(&self) {
        let ids = {
            let _lifecycle = self.lifecycle_gate.lock_from_taskvisor_callback();
            self.completion_barriers
                .lock()
                .pending
                .iter()
                .filter_map(|(id, pending)| pending.safe_without_barrier.then_some(*id))
                .collect::<Vec<_>>()
        };
        for tv_raw in ids {
            let admission = self.state.admit_state_write_from_taskvisor_callback(
                StateMutationEventCapacity::TaskAndRunChange,
            );
            let _lifecycle = self.lifecycle_gate.lock_from_taskvisor_callback();
            match admission {
                Ok(admission) => {
                    if let Some(finalization) = self.take_safe_pending_locked(tv_raw) {
                        self.finalize_admitted_locked(tv_raw, finalization, admission);
                    }
                }
                Err(_) => self.cleanup_without_projection_locked(tv_raw),
            }
        }
    }

    /// Releases every deferred outcome after Taskvisor has confirmed global shutdown.
    ///
    /// At that point no registered runtime remains. A missing per-task
    /// `TaskRemoved` event is no longer needed as cleanup evidence.
    pub(crate) async fn finalize_pending_after_confirmed_shutdown(&self) {
        let pending = {
            let _lifecycle = self.lifecycle_gate.lock().await;
            let mut barriers = self.completion_barriers.lock();
            barriers.pending.drain().collect::<Vec<_>>()
        };
        for (tv_raw, finalization) in pending {
            self.finalize_async(tv_raw, finalization).await;
        }
    }

    fn task_removed_admitted_locked(&self, tv_raw: u64, admission: StateWriteAdmission) {
        let pending = {
            let mut barriers = self.completion_barriers.lock();
            let pending = barriers.pending.remove(&tv_raw);
            if pending.is_none() {
                barriers.mark_removed(tv_raw);
            }
            pending
        };
        if let Some(finalization) = pending {
            self.finalize_admitted_locked(tv_raw, finalization, admission);
        }
    }

    async fn finalize_async(&self, tv_raw: u64, finalization: Finalization) {
        let admission = self
            .state
            .admit_state_write(StateMutationEventCapacity::TaskAndRunChange)
            .await;
        let _lifecycle = self.lifecycle_gate.lock().await;
        match admission {
            Ok(admission) => self.finalize_admitted_locked(tv_raw, finalization, admission),
            Err(_) => self.cleanup_without_projection_locked(tv_raw),
        }
    }

    /// Removes runtime ownership after authoritative cleanup when projection
    /// persistence is unavailable. This does not synthesize status or run data.
    fn cleanup_without_projection_locked(&self, tv_raw: u64) {
        if let Some(binding) = self.state.resolve_tv(tv_raw)
            && self.state.unbind_exact(&binding)
        {
            self.output_hub
                .evict_if_uid(&binding.resource.name, &binding.resource.uid);
        }
        self.mark_completed_locked(tv_raw);
    }

    fn finalize_admitted_locked(
        &self,
        tv_raw: u64,
        finalization: Finalization,
        admission: StateWriteAdmission,
    ) {
        if let Some(model_id) = self.state.finalize_if_bound_admitted(
            tv_raw,
            finalization.phase,
            finalization.error,
            finalization.exit_code,
            finalization.force,
            admission,
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
    fn event_state_capacity(event: &Event) -> StateMutationEventCapacity {
        if event.id.is_none() {
            return StateMutationEventCapacity::None;
        }
        match event.kind {
            EventKind::AttemptStarting
            | EventKind::AttemptSucceeded
            | EventKind::AttemptCanceled
            | EventKind::AttemptFailed
            | EventKind::AttemptTimedOut
                if event.attempt.is_some_and(|attempt| attempt > 0) =>
            {
                StateMutationEventCapacity::AttemptTransition
            }
            EventKind::TaskFinished if event.outcome_kind.is_some() => {
                StateMutationEventCapacity::TaskChange
            }
            EventKind::ControllerRejected | EventKind::TaskAddFailed => {
                StateMutationEventCapacity::TaskChange
            }
            EventKind::TaskRemoved => StateMutationEventCapacity::TaskAndRunChange,
            _ => StateMutationEventCapacity::None,
        }
    }

    /// Applies one pre-admitted event while holding the lifecycle gate.
    fn apply_event_admitted_locked(&self, event: &Event, admission: StateWriteAdmission) {
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
            self.task_removed_admitted_locked(tv_raw, admission);
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
                if self
                    .state
                    .transition_attempt_starting_admitted(&binding, attempt, admission)
                {
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
                if self.state.transition_attempt_finished_admitted(
                    &binding,
                    attempt,
                    TaskPhase::Succeeded,
                    None,
                    None,
                    admission,
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
                if self.state.transition_attempt_finished_admitted(
                    &binding,
                    attempt,
                    TaskPhase::Canceled,
                    None,
                    None,
                    admission,
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
                if self.state.transition_attempt_finished_admitted(
                    &binding,
                    attempt,
                    TaskPhase::Failed,
                    Some(reason),
                    event.exit_code,
                    admission,
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
                if self.state.transition_attempt_finished_admitted(
                    &binding,
                    attempt,
                    TaskPhase::Timeout,
                    Some(error),
                    None,
                    admission,
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
                    .transition_task_finished_admitted(&binding, phase, error, exit_code, admission)
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
                if !self.state.transition_task_finished_admitted(
                    &binding,
                    phase,
                    Some(reason),
                    None,
                    admission,
                ) {
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
                if !self.state.transition_task_finished_admitted(
                    &binding,
                    phase,
                    Some(reason),
                    None,
                    admission,
                ) {
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
        if event.kind == EventKind::SubscriberOverflow {
            if event.task.as_deref().is_some_and(|subscriber| {
                subscriber != self.name() && subscriber != "subscriber_listener"
            }) {
                return;
            }
            self.force_all_safe_pending_from_taskvisor_callback();
            return;
        }

        let Ok(admission) = self
            .state
            .admit_state_write_from_taskvisor_callback(Self::event_state_capacity(event))
        else {
            return;
        };
        let _lifecycle = self.lifecycle_gate.lock_from_taskvisor_callback();
        self.apply_event_admitted_locked(event, admission);
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
        let admission = self
            .state
            .admit_state_write_from_taskvisor_callback(StateMutationEventCapacity::TaskAndRunChange)
            .expect("test finalization requires open state persistence");
        let _lifecycle = self.lifecycle_gate.lock_from_taskvisor_callback();
        let (phase, error, exit_code) = phase_for_outcome(outcome);
        self.finalize_admitted_locked(
            tv_raw,
            Finalization {
                phase,
                error,
                exit_code,
                force: Self::is_known_outcome(outcome),
                safe_without_barrier: true,
            },
            admission,
        );
    }
}

#[cfg(test)]
mod tests;
