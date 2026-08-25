//! # Desired-state reconciliation
//!
//! [`Reconciler`] turns one committed task generation into a Taskvisor submission.
//!
//! ## Flow
//!
//! ```text
//! committed Task
//!      │ current UID and generation
//!      ▼
//! build runner task or use embedded TaskRef
//!      │
//!      ▼
//! map policies and prepare submission
//!      ├── failure ──► Reconciled=False
//!      ▼
//! current fence ──► stop previous binding ──► bind ──► submit and watch
//!                                                      ├── failure ──► Reconciled=False
//!                                                      └── success ──► Reconciled=True
//! ```
//!
//! Preflight runs outside the per-task runtime lock.
//! The generation is checked before preflight, cleanup, and binding.
//! A stale generation cannot acquire a new binding.
//!
//! A bound generation can submit while a newer apply commits.
//! A later successful reconciliation replaces that runtime.
//! Cleanup and binding failures also set `Reconciled=False`.
//! Controller intake can wait for Taskvisor ownership capacity.
//! Supersede, explicit cancellation, and shutdown cancel that wait and release
//! the unsubmitted binding. Explicit cancellation marks the current generation
//! `Reconciled=False`; supersede and shutdown preserve their existing status.
//!
//! Completion waiters provide the authoritative final outcome.
//! One coordinator worker per task keeps only the latest pending generation.
//! It retains tracker ownership while a child reconciliation task runs.
//! A provisional binding guard survives child unwind until exact cleanup.
//! Accepted waiters enter the tracker before reporting or status persistence.
//! Runner builds use bounded global and per-runner admission.
//! The task tracker lets shutdown cancel and drain every owned worker.

use std::{
    collections::{HashMap, hash_map::Entry},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use solti_model::{AgentCapabilities, Task, TaskId};
use solti_runner::{
    BuildCancellation, BuildCancellationHandle, BuiltTask, RouterError, RunnerBuildAdmission,
    RunnerRouter, make_run_id,
};
use taskvisor::{
    ControllerSpec, PreparedSubmission, SupervisorHandle, TaskRef, TaskSpec as TvTaskSpec,
};
use tokio::sync::oneshot;
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker, task_tracker::TaskTrackerToken},
};
use tracing::{debug, instrument, warn};

use super::{RuntimeObserver, TaskLocks, observer::ProvisionalBinding};
use crate::{
    CoreError, ReconciliationConfig, StateConfig,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    output::OutputHub,
    state::{ResourceGeneration, RuntimeBinding, StateMutationEventCapacity, TaskState},
};

pub(crate) const RUNTIME_SUBMISSION_CANCELLED_REASON: &str = "RuntimeSubmissionCancelled";
pub(crate) const RUNTIME_SUBMISSION_CANCELLED_MESSAGE: &str =
    "runtime submission was cancelled before Taskvisor intake completed";

/// Executable source for one reconciliation.
pub(crate) enum RuntimeSource {
    /// Builds the task through the registered runner.
    Routed,
    /// Uses a caller-owned Taskvisor task.
    Prebuilt(TaskRef),
}

impl RuntimeSource {
    fn as_label(&self) -> &'static str {
        match self {
            Self::Routed => "routed",
            Self::Prebuilt(_) => "prebuilt",
        }
    }
}

/// Structured context for destroying ownership that Taskvisor has not accepted.
#[derive(Clone)]
struct UnsubmittedDisposalContext {
    task_name: TaskId,
    task_uid: Option<String>,
    generation: Option<u64>,
    taskvisor_id: Option<taskvisor::TaskId>,
    ownership: &'static str,
    stage: &'static str,
}

impl UnsubmittedDisposalContext {
    fn new(
        resource: ResourceGeneration,
        taskvisor_id: Option<taskvisor::TaskId>,
        ownership: &'static str,
        stage: &'static str,
    ) -> Self {
        Self {
            task_name: resource.name,
            task_uid: Some(resource.uid.to_string()),
            generation: Some(resource.generation),
            taskvisor_id,
            ownership,
            stage,
        }
    }

    fn before_commit(task_name: TaskId, ownership: &'static str, stage: &'static str) -> Self {
        Self {
            task_name,
            task_uid: None,
            generation: None,
            taskvisor_id: None,
            ownership,
            stage,
        }
    }
}

/// Owns a pre-intake value whose destructor must never unwind into coordination.
pub(crate) struct NonUnwindingDrop<T> {
    value: Option<T>,
    context: UnsubmittedDisposalContext,
}

impl<T> NonUnwindingDrop<T> {
    fn new(value: T, context: UnsubmittedDisposalContext) -> Self {
        Self {
            value: Some(value),
            context,
        }
    }

    pub(crate) fn as_ref(&self) -> &T {
        self.value
            .as_ref()
            .expect("guarded ownership is present until transfer or disposal")
    }

    fn as_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("guarded ownership is present until transfer or disposal")
    }

    fn into_inner(mut self) -> T {
        self.value
            .take()
            .expect("guarded ownership transfers exactly once")
    }

    pub(crate) fn dispose_at(mut self, stage: &'static str) {
        self.context.stage = stage;
        if let Some(value) = self.value.take() {
            dispose_unsubmitted(value, &self.context);
        }
    }
}

impl<T> Drop for NonUnwindingDrop<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            dispose_unsubmitted(value, &self.context);
        }
    }
}

/// Destroys one pre-intake value without retaining or reporting its panic payload.
fn dispose_unsubmitted<T>(value: T, context: &UnsubmittedDisposalContext) {
    let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) else {
        return;
    };
    let nested_payload_drop_panicked = dispose_caught_panic_payload(payload);
    report_without_unwind(|| {
        warn!(
            event = "task.unsubmitted_disposal_panicked",
            task_name = %context.task_name,
            task_uid = ?context.task_uid,
            generation = ?context.generation,
            taskvisor_id = ?context.taskvisor_id.map(taskvisor::TaskId::get),
            ownership = context.ownership,
            stage = context.stage,
            nested_payload_drop_panicked,
            "unsubmitted runtime ownership disposal panicked"
        );
    });
}

pub(crate) type GuardedRuntimeSource = NonUnwindingDrop<RuntimeSource>;

pub(crate) fn guard_runtime_source(
    source: RuntimeSource,
    task_name: TaskId,
) -> GuardedRuntimeSource {
    NonUnwindingDrop::new(
        source,
        UnsubmittedDisposalContext::before_commit(
            task_name,
            "runtime_source",
            "before_desired_commit",
        ),
    )
}

/// Destroys one caught payload without allowing recursive destructor unwinding.
fn dispose_caught_panic_payload(payload: Box<dyn std::any::Any + Send>) -> bool {
    match catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        Ok(()) => false,
        Err(nested) => {
            // A hostile panic payload cannot be destroyed safely. Retain only
            // this one nested payload instead of attempting recursive cleanup.
            std::mem::forget(nested);
            true
        }
    }
}

/// Executes best-effort cleanup reporting without making logging a failure path.
fn report_without_unwind(report: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(report)) {
        let _ = dispose_caught_panic_payload(payload);
    }
}

struct ReconciliationRequest {
    desired: Task,
    source: NonUnwindingDrop<RuntimeSource>,
    ensure_output: bool,
    cancellation: ReconciliationCancellation,
    completion: oneshot::Sender<Task>,
    _registration: TaskTrackerToken,
}

/// Cancellation state shared by one queued or active reconciliation.
///
/// The runner receives only the build signal. Core separately records whether
/// an explicit `cancel_task` request caused that signal, because supersede,
/// delete, and shutdown have different observable status contracts.
#[derive(Clone)]
struct ReconciliationCancellation {
    handle: BuildCancellationHandle,
    signal: BuildCancellation,
    user_requested: Arc<AtomicBool>,
}

impl ReconciliationCancellation {
    fn new() -> Self {
        let (handle, signal) = BuildCancellation::pair();
        Self {
            handle,
            signal,
            user_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cancel(&self, user_requested: bool) {
        if user_requested {
            self.user_requested.store(true, Ordering::Release);
        }
        self.handle.cancel();
    }

    fn is_user_requested(&self) -> bool {
        self.user_requested.load(Ordering::Acquire)
    }

    fn is_cancelled(&self) -> bool {
        self.signal.is_cancelled()
    }

    async fn cancelled(&self) {
        self.signal.cancelled().await;
    }
}

/// User-owned source from a coalesced request that must be destroyed after the
/// supervisor releases its global spawn gate.
pub(crate) struct SupersededReconciliation {
    _source: NonUnwindingDrop<RuntimeSource>,
}

#[derive(Default)]
struct ReconciliationSlot {
    active_cancellation: Option<ReconciliationCancellation>,
    pending: Option<ReconciliationRequest>,
    settled: Arc<CancellationToken>,
}

#[derive(Default)]
struct ReconciliationQueue {
    slots: Mutex<HashMap<TaskId, ReconciliationSlot>>,
}

/// Removes one coordinator slot and wakes settlement waiters on every exit path.
struct CoordinatorSettlement {
    reconciler: Reconciler,
    queue: Arc<ReconciliationQueue>,
    name: TaskId,
    settled: Arc<CancellationToken>,
    intake_handoff: SharedIntakeHandoff,
    armed: bool,
}

enum SettlementStep {
    Continue,
    Settled,
    Missing,
    Replaced,
}

enum IntakeHandoff {
    Provisional(ProvisionalBinding),
    Accepted {
        provisional: ProvisionalBinding,
        waiter: taskvisor::TaskWaiter,
    },
    AcceptedRegistered,
}

type SharedIntakeHandoff = Arc<Mutex<Option<IntakeHandoff>>>;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedIntakePanicStage {
    AfterProvisionalBind,
    BeforeAcceptedWaiterHandoff,
    AfterAcceptedWaiterRegistration,
}

#[cfg(test)]
struct InjectedIntakePanic {
    name: TaskId,
    stage: InjectedIntakePanicStage,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

impl CoordinatorSettlement {
    fn new(
        reconciler: Reconciler,
        queue: Arc<ReconciliationQueue>,
        name: TaskId,
        settled: Arc<CancellationToken>,
        intake_handoff: SharedIntakeHandoff,
    ) -> Self {
        Self {
            reconciler,
            queue,
            name,
            settled,
            intake_handoff,
            armed: true,
        }
    }

    /// Completes one request and reports whether the coordinator is settled.
    fn complete_request(&mut self) -> bool {
        let step = {
            let mut slots = self.queue.slots.lock();
            match slots.get_mut(&self.name) {
                None => SettlementStep::Missing,
                Some(slot) if !Arc::ptr_eq(&slot.settled, &self.settled) => {
                    SettlementStep::Replaced
                }
                Some(slot) => {
                    slot.active_cancellation = None;
                    if slot.pending.is_some() {
                        SettlementStep::Continue
                    } else {
                        let _ = slots.remove(&self.name);
                        SettlementStep::Settled
                    }
                }
            }
        };
        match step {
            SettlementStep::Continue => false,
            SettlementStep::Settled => {
                self.armed = false;
                self.settled.cancel();
                true
            }
            SettlementStep::Missing => self.finish_missing_slot(),
            SettlementStep::Replaced => {
                self.armed = false;
                report_without_unwind(|| {
                    warn!(
                        event = "task.reconcile_slot_replaced",
                        task_name = %self.name,
                        "active reconciliation slot identity changed"
                    );
                });
                self.settled.cancel();
                true
            }
        }
    }

    fn finish_missing_slot(&mut self) -> bool {
        self.armed = false;
        report_without_unwind(|| {
            warn!(
                event = "task.reconcile_slot_unavailable",
                task_name = %self.name,
                "active reconciliation slot became unavailable"
            );
        });
        self.settled.cancel();
        true
    }

    fn recover_intake_handoff_without_unwind(&self) {
        let intake_handoff = { self.intake_handoff.lock().take() };
        if let Some(intake_handoff) = intake_handoff
            && let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                self.reconciler.recover_intake_handoff(intake_handoff);
            }))
        {
            let _ = dispose_caught_panic_payload(payload);
        }
    }
}

impl Drop for CoordinatorSettlement {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = {
            let mut slots = self.queue.slots.lock();
            let matches = slots
                .get(&self.name)
                .is_some_and(|slot| Arc::ptr_eq(&slot.settled, &self.settled));
            matches.then(|| slots.remove(&self.name)).flatten()
        };
        let Some(mut slot) = slot else {
            self.recover_intake_handoff_without_unwind();
            self.settled.cancel();
            return;
        };
        let pending = slot.pending.take();
        drop(slot.active_cancellation.take());
        drop(slot);

        if let Some(request) = pending {
            request.dispose_pending("coordinator_exit_pending_source");
        }
        self.recover_intake_handoff_without_unwind();
        self.settled.cancel();
        report_without_unwind(|| {
            warn!(
                event = "task.reconcile_coordinator_unavailable",
                task_name = %self.name,
                "reconciliation coordinator exited before normal settlement"
            );
        });
    }
}

impl ReconciliationRequest {
    fn dispose_pending(self, stage: &'static str) {
        let Self {
            desired,
            source,
            ensure_output: _,
            cancellation,
            completion,
            _registration,
        } = self;
        source.dispose_at(stage);
        drop(completion);
        drop(_registration);
        drop(cancellation);
        drop(desired);
    }
}

enum BuildOutcome {
    Built(BuiltTask),
    Failed(CoreError),
    Panicked,
    TimedOut,
    Cancelled,
    Unavailable(String),
}

/// Dependencies shared by reconciliation and completion workers.
#[derive(Clone)]
pub(crate) struct Reconciler {
    pub(crate) output_hub: Arc<OutputHub>,
    pub(crate) handle: SupervisorHandle,
    router: Arc<RunnerRouter>,
    pub(crate) state: TaskState,
    pub(crate) observer: Arc<RuntimeObserver>,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) tasks: TaskTracker,
    pub(crate) retention_stop: CancellationToken,
    pub(crate) preflight_stop: CancellationToken,
    pub(crate) runtime_operations: TaskLocks,
    pub(crate) grace: Duration,
    reconciliation_queue: Arc<ReconciliationQueue>,
    build_admission: RunnerBuildAdmission,
    build_timeout: Duration,
    #[cfg(test)]
    injected_intake_panic: Arc<Mutex<Option<InjectedIntakePanic>>>,
}

impl Reconciler {
    pub(crate) fn new(
        output_hub: Arc<OutputHub>,
        handle: SupervisorHandle,
        router: RunnerRouter,
        state: TaskState,
        observer: Arc<RuntimeObserver>,
        grace: Duration,
        reconciliation_config: ReconciliationConfig,
    ) -> Self {
        Self {
            output_hub,
            handle,
            router: Arc::new(router),
            state,
            observer,
            runtime: tokio::runtime::Handle::current(),
            tasks: TaskTracker::new(),
            retention_stop: CancellationToken::new(),
            preflight_stop: CancellationToken::new(),
            runtime_operations: TaskLocks::default(),
            grace,
            reconciliation_queue: Arc::new(ReconciliationQueue::default()),
            build_admission: RunnerBuildAdmission::new(
                reconciliation_config.max_concurrent_builds(),
                reconciliation_config.max_concurrent_builds_per_runner(),
            )
            .expect("ReconciliationConfig validates runner build admission limits"),
            build_timeout: reconciliation_config.build_timeout(),
            #[cfg(test)]
            injected_intake_panic: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn runner_capabilities(&self) -> AgentCapabilities {
        self.router.capabilities()
    }

    #[cfg(test)]
    fn arm_intake_panic(
        &self,
        name: TaskId,
        stage: InjectedIntakePanicStage,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let previous = self
            .injected_intake_panic
            .lock()
            .replace(InjectedIntakePanic {
                name,
                stage,
                entered,
                release: release_rx,
            });
        assert!(
            previous.is_none(),
            "only one injected intake panic may be armed"
        );
        (entered_rx, release)
    }

    #[cfg(test)]
    pub(crate) fn arm_after_provisional_bind_panic(
        &self,
        name: TaskId,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.arm_intake_panic(name, InjectedIntakePanicStage::AfterProvisionalBind)
    }

    #[cfg(test)]
    pub(crate) fn arm_before_accepted_waiter_handoff_panic(
        &self,
        name: TaskId,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.arm_intake_panic(name, InjectedIntakePanicStage::BeforeAcceptedWaiterHandoff)
    }

    #[cfg(test)]
    pub(crate) fn arm_after_accepted_waiter_registration_panic(
        &self,
        name: TaskId,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.arm_intake_panic(
            name,
            InjectedIntakePanicStage::AfterAcceptedWaiterRegistration,
        )
    }

    #[cfg(test)]
    async fn inject_intake_panic_if_armed(&self, name: &TaskId, stage: InjectedIntakePanicStage) {
        let injection = {
            let mut injection = self.injected_intake_panic.lock();
            let matches = injection
                .as_ref()
                .is_some_and(|injection| &injection.name == name && injection.stage == stage);
            matches.then(|| injection.take()).flatten()
        };
        let Some(injection) = injection else {
            return;
        };
        let _ = injection.entered.send(());
        let _ = injection.release.await;
        panic!("injected reconciliation panic at {stage:?}");
    }

    /// Schedules one committed generation.
    ///
    /// A task owns at most one active and one pending reconciliation. Scheduling
    /// a newer generation cancels active preflight or intake and replaces the
    /// pending request.
    pub(crate) fn schedule(
        &self,
        desired: Task,
        mut source: GuardedRuntimeSource,
        ensure_output: bool,
        registration: TaskTrackerToken,
    ) -> (oneshot::Receiver<Task>, Option<SupersededReconciliation>) {
        let name = desired.name().clone();
        let resource = ResourceGeneration::from_task(&desired);
        source.context = UnsubmittedDisposalContext::new(
            resource,
            None,
            "runtime_source",
            "pending_reconciliation",
        );
        let cancellation = ReconciliationCancellation::new();
        let (completion, receiver) = oneshot::channel();
        let request = ReconciliationRequest {
            desired,
            source,
            ensure_output,
            cancellation,
            completion,
            _registration: registration,
        };

        let (coordinator_settled, superseded) = {
            let mut slots = self.reconciliation_queue.slots.lock();
            match slots.entry(name.clone()) {
                Entry::Vacant(entry) => {
                    let settled = Arc::new(CancellationToken::new());
                    entry.insert(ReconciliationSlot {
                        active_cancellation: None,
                        pending: Some(request),
                        settled: Arc::clone(&settled),
                    });
                    (Some(settled), None)
                }
                Entry::Occupied(mut entry) => {
                    if let Some(active) = &entry.get().active_cancellation {
                        active.cancel(false);
                    }
                    let superseded = entry.get_mut().pending.replace(request);
                    (None, superseded)
                }
            }
        };

        let superseded = superseded.map(|superseded| {
            debug!(
                event = "task.reconcile_coalesced",
                task_name = %name,
                generation = superseded.desired.metadata().generation(),
                "pending reconciliation replaced by a newer generation"
            );
            let ReconciliationRequest {
                desired,
                source,
                completion,
                _registration,
                ..
            } = superseded;
            let current = self.state.get_retained(&name).unwrap_or(desired);
            let _ = completion.send(current);
            drop(_registration);
            SupersededReconciliation { _source: source }
        });

        if let Some(settled) = coordinator_settled {
            let reconciler = self.clone();
            let runtime = self.runtime.clone();
            let worker = self.tasks.spawn_on(
                async move {
                    reconciler.run_scheduled(name, settled).await;
                },
                &runtime,
            );
            drop(worker);
        }

        (receiver, superseded)
    }

    /// Cancels active preflight or intake and pending work for a deleted task.
    ///
    /// The returned token becomes cancelled after the per-name coordinator has
    /// released every request and user-owned runtime source in this slot.
    pub(crate) fn cancel_scheduled(&self, name: &TaskId) -> Option<CancellationToken> {
        self.request_scheduled_cancellation(name, false)
    }

    /// Cancels scheduled work for an explicit `cancel_task` request.
    ///
    /// A current generation that has not crossed Taskvisor intake records an
    /// observable reconciliation failure before the returned token settles.
    pub(crate) fn cancel_scheduled_for_user(&self, name: &TaskId) -> Option<CancellationToken> {
        self.request_scheduled_cancellation(name, true)
    }

    fn request_scheduled_cancellation(
        &self,
        name: &TaskId,
        user_requested: bool,
    ) -> Option<CancellationToken> {
        let slots = self.reconciliation_queue.slots.lock();
        let slot = slots.get(name)?;
        if let Some(active) = &slot.active_cancellation {
            active.cancel(user_requested);
        }
        if let Some(pending) = &slot.pending {
            pending.cancellation.cancel(user_requested);
        }
        Some(slot.settled.as_ref().clone())
    }

    async fn run_scheduled(&self, name: TaskId, settled: Arc<CancellationToken>) {
        let intake_handoff = Arc::new(Mutex::new(None));
        let mut settlement = CoordinatorSettlement::new(
            self.clone(),
            Arc::clone(&self.reconciliation_queue),
            name.clone(),
            settled,
            Arc::clone(&intake_handoff),
        );
        loop {
            let request = {
                let mut slots = self.reconciliation_queue.slots.lock();
                let slot = slots
                    .get_mut(&name)
                    .expect("scheduled reconciliation slot exists");
                let request = slot
                    .pending
                    .take()
                    .expect("scheduled reconciliation request exists");
                slot.active_cancellation = Some(request.cancellation.clone());
                request
            };

            let ReconciliationRequest {
                desired,
                source,
                ensure_output,
                cancellation,
                completion,
                _registration,
            } = request;
            let fallback = desired.clone();
            let fallback_target = ResourceGeneration::from_task(&fallback);
            let recovery_cancellation = cancellation.clone();
            let child_reconciler = self.clone();
            let child_handoff = Arc::clone(&intake_handoff);
            let child = self.runtime.spawn(async move {
                child_reconciler
                    .reconcile_with_cancellation(
                        desired,
                        source,
                        ensure_output,
                        cancellation,
                        child_handoff,
                    )
                    .await
            });
            let result = match child.await {
                Ok(result) => result,
                Err(error) => {
                    let accepted = intake_handoff
                        .lock()
                        .take()
                        .is_some_and(|intake_handoff| self.recover_intake_handoff(intake_handoff));
                    if error.is_panic() {
                        let nested_payload_drop_panicked =
                            dispose_caught_panic_payload(error.into_panic());
                        report_without_unwind(|| {
                            warn!(
                                event = "task.reconcile_coordinator_child_panicked",
                                task_name = %name,
                                nested_payload_drop_panicked,
                                "reconciliation child exited with a panic"
                            );
                        });
                    } else {
                        report_without_unwind(|| {
                            warn!(
                                event = "task.reconcile_coordinator_child_unavailable",
                                task_name = %name,
                                error = %error,
                                "reconciliation child became unavailable"
                            );
                        });
                    }
                    if accepted {
                        self.state.get_retained(&name).unwrap_or(fallback)
                    } else {
                        self.current_after_cancellation(
                            &fallback_target,
                            fallback,
                            &recovery_cancellation,
                        )
                        .await
                    }
                }
            };
            if let Some(intake_handoff) = intake_handoff.lock().take() {
                self.recover_intake_handoff(intake_handoff);
            }
            let _ = completion.send(result);
            drop(_registration);

            if settlement.complete_request() {
                return;
            }
        }
    }

    pub(crate) fn spawn_retention_worker(&self, config: StateConfig) {
        let state = self.state.clone();
        let stop = self.retention_stop.clone();
        let interval = config.sweep_interval();
        let Some(start) = tokio::time::Instant::now().checked_add(interval) else {
            warn!(
                event = "task.retention_deadline_unrepresentable",
                interval_ms = interval.as_millis(),
                "retention worker deadline is not representable on this platform"
            );
            return;
        };
        let runtime = self.runtime.clone();
        let worker = self.tasks.spawn_on(
            async move {
                let mut ticker = tokio::time::interval_at(start, interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = ticker.tick() => {
                            state.sweep_async(&config).await;
                        }
                    }
                }
            },
            &runtime,
        );
        drop(worker);
    }

    fn failure_reason(error: &CoreError) -> &'static str {
        match error {
            CoreError::Runner(solti_runner::RouterError::NoRunner { .. }) => "RunnerNotFound",
            CoreError::Runner(solti_runner::RouterError::EmbeddedWorkload) => "WorkloadNotRoutable",
            CoreError::Runner(_) => "RunnerBuildFailed",
            CoreError::Mapping(_) => "PolicyMappingFailed",
            CoreError::Supervisor { op: "prepare", .. } => "RuntimePreparationFailed",
            _ => "ReconciliationFailed",
        }
    }

    fn dispose_interrupted_build(
        result: Result<Result<BuiltTask, CoreError>, tokio::task::JoinError>,
        target: &ResourceGeneration,
        stage: &'static str,
    ) {
        NonUnwindingDrop::new(
            result,
            UnsubmittedDisposalContext::new(target.clone(), None, "runner_build_result", stage),
        )
        .dispose_at(stage);
    }

    fn prepare_submission(
        &self,
        task: &Task,
        runtime_name: String,
        task_ref: TaskRef,
    ) -> Result<PreparedSubmission, CoreError> {
        let spec = task.spec();
        let task_spec = TvTaskSpec::new(
            runtime_name,
            task_ref,
            to_restart_policy(spec.restart())?,
            to_backoff_policy(spec.backoff())?,
            Some(Duration::from_millis(spec.timeout().as_millis())),
        )
        .with_max_retries(spec.max_retries());
        let controller_spec =
            ControllerSpec::new(to_admission_policy(spec.admission())?, task_spec)
                .with_slot(spec.slot().as_str());

        self.handle
            .prepare_submission(controller_spec)
            .map_err(|error| CoreError::supervisor("prepare", error))
    }

    async fn build_routed(
        &self,
        desired: &Task,
        cancellation: &ReconciliationCancellation,
    ) -> BuildOutcome {
        let target = ResourceGeneration::from_task(desired);
        let Some(deadline) = tokio::time::Instant::now().checked_add(self.build_timeout) else {
            cancellation.cancel(false);
            warn!(
                event = "task.runner_build_deadline_unrepresentable",
                timeout_ms = self.build_timeout.as_millis(),
                "runner build deadline is not representable on this platform"
            );
            return BuildOutcome::TimedOut;
        };
        let deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline);
        let admitted = tokio::select! {
            biased;
            _ = self.preflight_stop.cancelled() => {
                cancellation.cancel(false);
                return BuildOutcome::Cancelled;
            }
            _ = cancellation.cancelled() => {
                return BuildOutcome::Cancelled;
            }
            _ = &mut deadline => {
                cancellation.cancel(false);
                return BuildOutcome::TimedOut;
            }
            admitted = self.router.admit(
                desired,
                &self.build_admission,
                cancellation.signal.clone(),
            ) => admitted,
        };
        let admitted = match admitted {
            Ok(admitted) => admitted,
            Err(RouterError::BuildCancelled { .. }) => return BuildOutcome::Cancelled,
            Err(error) => return BuildOutcome::Failed(CoreError::from(error)),
        };
        if self.preflight_stop.is_cancelled() || cancellation.is_cancelled() {
            return BuildOutcome::Cancelled;
        }

        let mut build = AbortOnDropHandle::new(
            self.runtime
                .spawn(async move { admitted.build().await.map_err(CoreError::from) }),
        );

        let result = tokio::select! {
            biased;
            _ = self.preflight_stop.cancelled() => {
                cancellation.cancel(false);
                build.abort();
                Self::dispose_interrupted_build(
                    build.await,
                    &target,
                    "shutdown_during_runner_build",
                );
                return BuildOutcome::Cancelled;
            }
            _ = cancellation.cancelled() => {
                build.abort();
                Self::dispose_interrupted_build(
                    build.await,
                    &target,
                    "cancelled_during_runner_build",
                );
                return BuildOutcome::Cancelled;
            }
            _ = &mut deadline => {
                cancellation.cancel(false);
                build.abort();
                Self::dispose_interrupted_build(
                    build.await,
                    &target,
                    "runner_build_timed_out",
                );
                return BuildOutcome::TimedOut;
            }
            result = &mut build => result,
        };

        match result {
            Ok(Ok(built_task)) => BuildOutcome::Built(built_task),
            Ok(Err(error)) => BuildOutcome::Failed(error),
            Err(error) if error.is_panic() => {
                let nested_payload_drop_panicked = dispose_caught_panic_payload(error.into_panic());
                warn!(
                    event = "task.runner_build_panicked",
                    task_name = %target.name,
                    task_uid = %target.uid,
                    generation = target.generation,
                    nested_payload_drop_panicked,
                    "runner build task panicked"
                );
                BuildOutcome::Panicked
            }
            Err(error) => BuildOutcome::Unavailable(error.to_string()),
        }
    }

    /// Acquires one root build admission for a deterministic test.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when runner selection or admission fails.
    #[cfg(test)]
    pub(crate) async fn admit_for_test(
        &self,
        desired: &Task,
    ) -> Result<solti_runner::AdmittedBuild, RouterError> {
        self.router
            .admit(desired, &self.build_admission, BuildCancellation::new())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn reconcile(
        &self,
        desired: Task,
        source: RuntimeSource,
        ensure_output: bool,
    ) -> Task {
        let cancellation = ReconciliationCancellation::new();
        let resource = ResourceGeneration::from_task(&desired);
        let source = NonUnwindingDrop::new(
            source,
            UnsubmittedDisposalContext::new(
                resource,
                None,
                "runtime_source",
                "direct_reconciliation",
            ),
        );
        self.reconcile_with_cancellation(
            desired,
            source,
            ensure_output,
            cancellation,
            Arc::new(Mutex::new(None)),
        )
        .await
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(
            event = "task.reconcile",
            task_name = %desired.name(),
            task_uid = %desired.uid(),
            generation = desired.metadata().generation(),
            runtime_source = source.as_ref().as_label(),
        )
    )]
    async fn reconcile_with_cancellation(
        &self,
        desired: Task,
        source: NonUnwindingDrop<RuntimeSource>,
        ensure_output: bool,
        cancellation: ReconciliationCancellation,
        intake_handoff: SharedIntakeHandoff,
    ) -> Task {
        let target = ResourceGeneration::from_task(&desired);
        if !self.state.is_current(&target) {
            source.dispose_at("stale_before_preflight");
            return self.current(&target, desired);
        }
        if cancellation.is_cancelled() {
            source.dispose_at("cancelled_before_preflight");
            return self
                .current_after_cancellation(&target, desired, &cancellation)
                .await;
        }
        if self.preflight_stop.is_cancelled() {
            source.dispose_at("shutdown_before_preflight");
            return self.current(&target, desired);
        }

        let (runtime_name, task_anchor) = match source.into_inner() {
            RuntimeSource::Prebuilt(task_ref) => {
                let task_anchor = NonUnwindingDrop::new(
                    task_ref,
                    UnsubmittedDisposalContext::new(
                        target.clone(),
                        None,
                        "task_ref_anchor",
                        "embedded_preparation",
                    ),
                );
                (
                    make_run_id("embedded", desired.slot().as_str()).into_name(),
                    task_anchor,
                )
            }
            RuntimeSource::Routed => match self.build_routed(&desired, &cancellation).await {
                BuildOutcome::Built(built_task) => {
                    let (run_id, task_ref) = built_task.into_parts();
                    let task_anchor = NonUnwindingDrop::new(
                        task_ref,
                        UnsubmittedDisposalContext::new(
                            target.clone(),
                            None,
                            "task_ref_anchor",
                            "routed_preparation",
                        ),
                    );
                    (run_id.into_name(), task_anchor)
                }
                BuildOutcome::Failed(error) => {
                    let admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange)
                        .await;
                    let Ok(admission) = admission else {
                        return self.current(&target, desired);
                    };
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        Self::failure_reason(&error),
                        error.to_string(),
                        admission,
                    );
                    return self.current(&target, desired);
                }
                BuildOutcome::Panicked => {
                    warn!(
                        event = "task.reconcile_failed",
                        task_name = %target.name,
                        task_uid = %target.uid,
                        generation = target.generation,
                        error_kind = "runner_build_panicked",
                        "runner preflight panicked"
                    );
                    let admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange)
                        .await;
                    let Ok(admission) = admission else {
                        return self.current(&target, desired);
                    };
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        "RunnerBuildPanicked",
                        "reconciliation preflight panicked".to_string(),
                        admission,
                    );
                    return self.current(&target, desired);
                }
                BuildOutcome::TimedOut => {
                    warn!(
                        event = "task.reconcile_failed",
                        task_name = %target.name,
                        task_uid = %target.uid,
                        generation = target.generation,
                        error_kind = "runner_build_timed_out",
                        timeout_ms = self.build_timeout.as_millis(),
                        "runner preflight exceeded its deadline"
                    );
                    let admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange)
                        .await;
                    let Ok(admission) = admission else {
                        return self.current(&target, desired);
                    };
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        "RunnerBuildTimedOut",
                        format!(
                            "runner build exceeded {} ms deadline",
                            self.build_timeout.as_millis()
                        ),
                        admission,
                    );
                    return self.current(&target, desired);
                }
                BuildOutcome::Cancelled => {
                    return self
                        .current_after_cancellation(&target, desired, &cancellation)
                        .await;
                }
                BuildOutcome::Unavailable(error) => {
                    warn!(
                        event = "task.reconcile_failed",
                        task_name = %target.name,
                        task_uid = %target.uid,
                        generation = target.generation,
                        error_kind = "runner_build_unavailable",
                        error = %error,
                        "runner preflight unavailable"
                    );
                    let admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange)
                        .await;
                    let Ok(admission) = admission else {
                        return self.current(&target, desired);
                    };
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        "RunnerBuildUnavailable",
                        "reconciliation preflight worker was unavailable".to_string(),
                        admission,
                    );
                    return self.current(&target, desired);
                }
            },
        };

        if cancellation.is_cancelled() {
            task_anchor.dispose_at("cancelled_before_runtime_preparation");
            return self
                .current_after_cancellation(&target, desired, &cancellation)
                .await;
        }
        if self.preflight_stop.is_cancelled() {
            task_anchor.dispose_at("shutdown_before_runtime_preparation");
            return self.current(&target, desired);
        }
        let prepared = match catch_unwind(AssertUnwindSafe(|| {
            self.prepare_submission(&desired, runtime_name, Arc::clone(task_anchor.as_ref()))
        })) {
            Ok(Ok(prepared)) => {
                let taskvisor_id = prepared.id();
                NonUnwindingDrop::new(
                    prepared,
                    UnsubmittedDisposalContext::new(
                        target.clone(),
                        Some(taskvisor_id),
                        "prepared_submission",
                        "prepared",
                    ),
                )
            }
            Ok(Err(error)) => {
                task_anchor.dispose_at("runtime_preparation_failed");
                let admission = self
                    .state
                    .admit_state_write(StateMutationEventCapacity::TaskChange)
                    .await;
                let Ok(admission) = admission else {
                    return self.current(&target, desired);
                };
                self.state.mark_reconciliation_failed_admitted(
                    &target,
                    Self::failure_reason(&error),
                    error.to_string(),
                    admission,
                );
                return self.current(&target, desired);
            }
            Err(payload) => {
                let nested_payload_drop_panicked = dispose_caught_panic_payload(payload);
                warn!(
                    event = "task.reconcile_failed",
                    task_name = %target.name,
                    task_uid = %target.uid,
                    generation = target.generation,
                    error_kind = "runtime_preparation_panicked",
                    nested_payload_drop_panicked,
                    "runtime preparation panicked"
                );
                task_anchor.dispose_at("runtime_preparation_panicked");
                let admission = self
                    .state
                    .admit_state_write(StateMutationEventCapacity::TaskChange)
                    .await;
                let Ok(admission) = admission else {
                    return self.current(&target, desired);
                };
                self.state.mark_reconciliation_failed_admitted(
                    &target,
                    "RuntimePreparationPanicked",
                    "runtime preparation panicked before Taskvisor intake".to_string(),
                    admission,
                );
                return self.current(&target, desired);
            }
        };

        let _runtime_operation = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                prepared.dispose_at("cancelled_while_waiting_for_runtime_lock");
                task_anchor.dispose_at("cancelled_while_waiting_for_runtime_lock");
                return self
                    .current_after_cancellation(&target, desired, &cancellation)
                    .await;
            }
            _ = self.preflight_stop.cancelled() => {
                prepared.dispose_at("shutdown_while_waiting_for_runtime_lock");
                task_anchor.dispose_at("shutdown_while_waiting_for_runtime_lock");
                return self.current(&target, desired);
            }
            operation = self.runtime_operations.lock(&target.name) => operation,
        };
        if cancellation.is_cancelled() {
            prepared.dispose_at("cancelled_after_runtime_lock");
            task_anchor.dispose_at("cancelled_after_runtime_lock");
            return self
                .current_after_cancellation(&target, desired, &cancellation)
                .await;
        }
        if self.preflight_stop.is_cancelled() || !self.state.is_current(&target) {
            prepared.dispose_at("stale_after_runtime_lock");
            task_anchor.dispose_at("stale_after_runtime_lock");
            return self.current(&target, desired);
        }

        if let Some(previous) = self.state.binding_for(&target.name)
            && previous.resource != target
        {
            match self
                .handle
                .cancel_with_timeout(
                    previous.tv,
                    self.grace.saturating_add(Duration::from_secs(1)),
                )
                .await
            {
                Ok(_) => {
                    self.observer
                        .settle_after_confirmed_cleanup(previous.tv)
                        .await;
                }
                Err(error) => {
                    prepared.dispose_at("previous_runtime_cleanup_failed");
                    task_anchor.dispose_at("previous_runtime_cleanup_failed");
                    let admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange)
                        .await;
                    let Ok(admission) = admission else {
                        return self.current(&target, desired);
                    };
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        "PreviousRuntimeCleanupFailed",
                        CoreError::supervisor("cancel", error).to_string(),
                        admission,
                    );
                    return self.current(&target, desired);
                }
            }
        }

        if cancellation.is_cancelled() {
            prepared.dispose_at("cancelled_after_previous_runtime_cleanup");
            task_anchor.dispose_at("cancelled_after_previous_runtime_cleanup");
            return self
                .current_after_cancellation(&target, desired, &cancellation)
                .await;
        }
        if self.preflight_stop.is_cancelled() || !self.state.is_current(&target) {
            prepared.dispose_at("stale_after_previous_runtime_cleanup");
            task_anchor.dispose_at("stale_after_previous_runtime_cleanup");
            return self.current(&target, desired);
        }

        let tv = prepared.as_ref().id();
        let Some(provisional) = self.observer.bind(target.clone(), tv, ensure_output).await else {
            prepared.dispose_at("runtime_binding_failed");
            task_anchor.dispose_at("runtime_binding_failed");
            let admission = self
                .state
                .admit_state_write(StateMutationEventCapacity::TaskChange)
                .await;
            let Ok(admission) = admission else {
                return self.current(&target, desired);
            };
            self.state.mark_reconciliation_failed_admitted(
                &target,
                "RuntimeBindingFailed",
                "resource changed before runtime binding".to_string(),
                admission,
            );
            return self.current(&target, desired);
        };
        {
            let mut handoff = intake_handoff.lock();
            if handoff.is_some() {
                drop(handoff);
                drop(provisional);
                panic!("intake handoff must be empty before binding");
            }
            *handoff = Some(IntakeHandoff::Provisional(provisional));
        }

        #[cfg(test)]
        self.inject_intake_panic_if_armed(
            &target.name,
            InjectedIntakePanicStage::AfterProvisionalBind,
        )
        .await;

        let admission = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                prepared.dispose_at("cancelled_before_intake_status_admission");
                task_anchor.dispose_at("cancelled_before_intake_status_admission");
                self.release_provisional_handoff(&intake_handoff).await;
                return self
                    .current_after_cancellation(&target, desired, &cancellation)
                    .await;
            }
            _ = self.preflight_stop.cancelled() => {
                prepared.dispose_at("shutdown_before_intake_status_admission");
                task_anchor.dispose_at("shutdown_before_intake_status_admission");
                self.release_provisional_handoff(&intake_handoff).await;
                return self.current(&target, desired);
            }
            admission = self
                .state
                .admit_state_write(StateMutationEventCapacity::TaskChange) => admission,
        };
        let Ok(admission) = admission else {
            prepared.dispose_at("intake_status_admission_closed");
            task_anchor.dispose_at("intake_status_admission_closed");
            self.release_provisional_handoff(&intake_handoff).await;
            return self.current(&target, desired);
        };
        let intake_is_pending = self
            .state
            .ensure_taskvisor_intake_pending_admitted(&target, admission);
        if !intake_is_pending || !self.state.is_current(&target) {
            prepared.dispose_at("stale_before_taskvisor_intake");
            task_anchor.dispose_at("stale_before_taskvisor_intake");
            self.release_provisional_handoff(&intake_handoff).await;
            return self.current(&target, desired);
        }

        let intake_started = tokio::time::Instant::now();
        report_without_unwind(|| {
            debug!(
                event = "task.taskvisor_intake_wait_started",
                task_name = %target.name,
                task_uid = %target.uid,
                generation = target.generation,
                taskvisor_id = tv.get(),
                wait_scope = "ownership_and_controller_intake",
                "waiting for Taskvisor ownership and controller intake capacity"
            );
        });

        let submission = Box::pin(prepared.into_inner().submit_and_watch());
        let mut submission = NonUnwindingDrop::new(
            submission,
            UnsubmittedDisposalContext::new(
                target.clone(),
                Some(tv),
                "submission_future",
                "taskvisor_intake",
            ),
        );
        let mut completed_submission = None;
        let cancelled_by_user = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                Some(true)
            }
            _ = self.preflight_stop.cancelled() => {
                Some(false)
            }
            result = submission.as_mut().as_mut() => {
                completed_submission = Some(result);
                None
            }
        };

        if let Some(cancelled_by_user) = cancelled_by_user {
            submission.dispose_at("cancelled_during_taskvisor_intake");
            task_anchor.dispose_at("cancelled_during_taskvisor_intake");
            report_without_unwind(|| {
                debug!(
                    event = "task.taskvisor_intake_wait_finished",
                    task_name = %target.name,
                    task_uid = %target.uid,
                    generation = target.generation,
                    taskvisor_id = tv.get(),
                    wait_scope = "ownership_and_controller_intake",
                    outcome = "cancelled",
                    elapsed_ms = u64::try_from(intake_started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                    "Taskvisor intake wait cancelled"
                );
            });
            self.release_provisional_handoff(&intake_handoff).await;
            if cancelled_by_user {
                return self
                    .current_after_cancellation(&target, desired, &cancellation)
                    .await;
            }
            return self.current(&target, desired);
        }

        let submission_result =
            completed_submission.expect("Taskvisor intake completed without cancellation");
        submission.dispose_at("taskvisor_intake_completed");
        task_anchor.dispose_at("taskvisor_intake_completed");
        match submission_result {
            Ok((submitted, waiter)) => {
                let accepted_binding = RuntimeBinding {
                    resource: target.clone(),
                    tv,
                };
                if !self.stage_accepted_handoff(&intake_handoff, accepted_binding, waiter) {
                    return self.current(&target, desired);
                }

                #[cfg(test)]
                self.inject_intake_panic_if_armed(
                    &target.name,
                    InjectedIntakePanicStage::BeforeAcceptedWaiterHandoff,
                )
                .await;

                if !self.handoff_accepted_waiter(&intake_handoff) {
                    return self.current(&target, desired);
                }

                #[cfg(test)]
                self.inject_intake_panic_if_armed(
                    &target.name,
                    InjectedIntakePanicStage::AfterAcceptedWaiterRegistration,
                )
                .await;

                report_without_unwind(|| {
                    debug!(
                        event = "task.taskvisor_intake_wait_finished",
                        task_name = %target.name,
                        task_uid = %target.uid,
                        generation = target.generation,
                        taskvisor_id = tv.get(),
                        submitted_taskvisor_id = submitted.get(),
                        wait_scope = "ownership_and_controller_intake",
                        outcome = "accepted",
                        elapsed_ms = u64::try_from(intake_started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        "Taskvisor intake wait finished"
                    );
                });
                let admission = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return self.current(&target, desired);
                    }
                    _ = self.preflight_stop.cancelled() => {
                        return self.current(&target, desired);
                    }
                    admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange) => admission,
                };
                if let Ok(admission) = admission {
                    self.state.mark_observed_admitted(&target, admission);
                }
            }
            Err(error) => {
                self.release_provisional_handoff(&intake_handoff).await;
                report_without_unwind(|| {
                    debug!(
                        event = "task.taskvisor_intake_wait_finished",
                        task_name = %target.name,
                        task_uid = %target.uid,
                        generation = target.generation,
                        taskvisor_id = tv.get(),
                        wait_scope = "ownership_and_controller_intake",
                        outcome = "failed",
                        elapsed_ms = u64::try_from(intake_started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        "Taskvisor intake wait finished"
                    );
                });
                let message = CoreError::supervisor("submit", error).to_string();
                let admission = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return self
                            .current_after_cancellation(&target, desired, &cancellation)
                            .await;
                    }
                    _ = self.preflight_stop.cancelled() => {
                        return self.current(&target, desired);
                    }
                    admission = self
                        .state
                        .admit_state_write(StateMutationEventCapacity::TaskChange) => admission,
                };
                if let Ok(admission) = admission {
                    self.state.mark_reconciliation_failed_admitted(
                        &target,
                        "RuntimeSubmissionFailed",
                        message,
                        admission,
                    );
                }
            }
        }
        self.current(&target, desired)
    }

    async fn current_after_cancellation(
        &self,
        target: &ResourceGeneration,
        fallback: Task,
        cancellation: &ReconciliationCancellation,
    ) -> Task {
        if cancellation.is_user_requested() && self.state.is_current(target) {
            let admission = self
                .state
                .admit_state_write(StateMutationEventCapacity::TaskChange)
                .await;
            if let Ok(admission) = admission {
                self.state.mark_reconciliation_failed_admitted(
                    target,
                    RUNTIME_SUBMISSION_CANCELLED_REASON,
                    RUNTIME_SUBMISSION_CANCELLED_MESSAGE.to_string(),
                    admission,
                );
            }
        }
        self.current(target, fallback)
    }

    fn current(&self, target: &ResourceGeneration, fallback: Task) -> Task {
        self.state.get_retained(&target.name).unwrap_or(fallback)
    }

    async fn release_provisional_handoff(&self, handoff: &SharedIntakeHandoff) -> bool {
        let provisional = {
            let mut handoff = handoff.lock();
            match handoff.take() {
                Some(IntakeHandoff::Provisional(provisional)) => provisional,
                unexpected => {
                    *handoff = unexpected;
                    return false;
                }
            }
        };
        provisional.release(self.observer.as_ref()).await
    }

    fn stage_accepted_handoff(
        &self,
        handoff: &SharedIntakeHandoff,
        fallback_binding: RuntimeBinding,
        waiter: taskvisor::TaskWaiter,
    ) -> bool {
        let mut slot = handoff.lock();
        let provisional = match slot.take() {
            Some(IntakeHandoff::Provisional(provisional)) => provisional,
            unexpected => {
                let was_empty = unexpected.is_none();
                *slot = unexpected;
                drop(slot);
                self.spawn_completion_waiter(fallback_binding, waiter);
                if was_empty {
                    let mut slot = handoff.lock();
                    if slot.is_none() {
                        *slot = Some(IntakeHandoff::AcceptedRegistered);
                    }
                }
                return false;
            }
        };
        *slot = Some(IntakeHandoff::Accepted {
            provisional,
            waiter,
        });
        true
    }

    fn handoff_accepted_waiter(&self, handoff: &SharedIntakeHandoff) -> bool {
        let accepted = {
            let mut handoff = handoff.lock();
            match handoff.take() {
                Some(IntakeHandoff::Accepted {
                    provisional,
                    waiter,
                }) => (provisional, waiter),
                unexpected => {
                    *handoff = unexpected;
                    return false;
                }
            }
        };
        let (provisional, waiter) = accepted;
        self.register_accepted_waiter(provisional, waiter);
        let mut handoff = handoff.lock();
        if handoff.is_none() {
            *handoff = Some(IntakeHandoff::AcceptedRegistered);
        }
        true
    }

    fn register_accepted_waiter(
        &self,
        provisional: ProvisionalBinding,
        waiter: taskvisor::TaskWaiter,
    ) {
        let binding = provisional.binding().clone();
        self.spawn_completion_waiter(binding, waiter);
        provisional.disarm();
    }

    fn recover_intake_handoff(&self, handoff: IntakeHandoff) -> bool {
        match handoff {
            IntakeHandoff::Provisional(provisional) => {
                drop(provisional);
                false
            }
            IntakeHandoff::Accepted {
                provisional,
                waiter,
            } => {
                self.register_accepted_waiter(provisional, waiter);
                true
            }
            IntakeHandoff::AcceptedRegistered => true,
        }
    }

    fn spawn_completion_waiter(&self, binding: RuntimeBinding, waiter: taskvisor::TaskWaiter) {
        let observer = Arc::clone(&self.observer);
        let cleanup_handle = self.handle.clone();
        let cleanup_timeout = self.grace.saturating_add(Duration::from_secs(1));
        let tv = binding.tv;
        let tv_raw = tv.get();
        let task_name = binding.resource.name.clone();
        let task_uid = binding.resource.uid.clone();
        let generation = binding.resource.generation;
        let task = self.tasks.spawn_on(
            async move {
                match waiter.wait().await {
                    Ok(outcome) => observer.finalize_from_outcome(tv_raw, &outcome).await,
                    Err(error) => {
                        warn!(
                            event = "task.outcome_unavailable",
                            task_name = %task_name,
                            task_uid = %task_uid,
                            generation,
                            taskvisor_id = tv_raw,
                            error = %error,
                            "task completion channel closed without an outcome"
                        );
                        let unavailable = format!("task outcome unavailable: {error}");
                        match cleanup_handle
                            .cancel_with_timeout(tv, cleanup_timeout)
                            .await
                        {
                            Ok(_) => {
                                observer
                                    .finalize_unavailable_after_cleanup(tv_raw, unavailable)
                                    .await;
                            }
                            Err(cleanup_error) => {
                                warn!(
                                    event = "task.cleanup_unconfirmed",
                                    task_name = %task_name,
                                    task_uid = %task_uid,
                                    generation,
                                    taskvisor_id = tv_raw,
                                    error = %cleanup_error,
                                    "could not confirm cleanup after task outcome became unavailable"
                                );
                                observer.finalize_unavailable(tv_raw, unavailable).await;
                            }
                        }
                    }
                }
            },
            &self.runtime,
        );
        drop(task);
    }
}

#[cfg(test)]
mod disposal_tests {
    use super::*;

    struct PanickingReportPayload;

    impl Drop for PanickingReportPayload {
        fn drop(&mut self) {
            panic!("nested report payload destructor");
        }
    }

    #[test]
    fn cleanup_reporting_contains_nested_panic_payload_destruction() {
        report_without_unwind(|| std::panic::panic_any(PanickingReportPayload));
    }
}
