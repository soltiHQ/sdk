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
//!
//! Completion waiters provide the authoritative final outcome.
//! One coordinator worker per task keeps only the latest pending generation.
//! Runner builds use bounded global and per-runner admission.
//! The task tracker lets shutdown cancel and drain every owned worker.

use std::{
    collections::{HashMap, hash_map::Entry},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use solti_model::{Task, TaskId};
use solti_runner::{
    BuildCancellation, BuildCancellationHandle, RouterError, RunnerBuildAdmission, RunnerRouter,
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

use super::{RuntimeObserver, TaskLocks};
use crate::{
    CoreError, ReconciliationConfig, StateConfig,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    output::OutputHub,
    state::{ResourceGeneration, RuntimeBinding, TaskState},
};

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

struct ReconciliationRequest {
    desired: Task,
    source: RuntimeSource,
    ensure_output: bool,
    cancel_handle: BuildCancellationHandle,
    cancellation: BuildCancellation,
    completion: oneshot::Sender<Task>,
    _registration: TaskTrackerToken,
}

/// User-owned source from a coalesced request that must be destroyed after the
/// supervisor releases its global spawn gate.
pub(crate) struct SupersededReconciliation {
    _source: RuntimeSource,
}

#[derive(Default)]
struct ReconciliationSlot {
    active_cancellation: Option<BuildCancellationHandle>,
    pending: Option<ReconciliationRequest>,
}

#[derive(Default)]
struct ReconciliationQueue {
    slots: Mutex<HashMap<TaskId, ReconciliationSlot>>,
}

enum BuildOutcome {
    Built(TaskRef),
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
        }
    }

    /// Schedules one committed generation.
    ///
    /// A task owns at most one active and one pending reconciliation. Scheduling
    /// a newer generation cancels active preflight and replaces the pending request.
    pub(crate) fn schedule(
        &self,
        desired: Task,
        source: RuntimeSource,
        ensure_output: bool,
        registration: TaskTrackerToken,
    ) -> (oneshot::Receiver<Task>, Option<SupersededReconciliation>) {
        let name = desired.name().clone();
        let (cancel_handle, cancellation) = BuildCancellation::pair();
        let (completion, receiver) = oneshot::channel();
        let request = ReconciliationRequest {
            desired,
            source,
            ensure_output,
            cancel_handle,
            cancellation,
            completion,
            _registration: registration,
        };

        let (spawn_worker, superseded) = {
            let mut slots = self.reconciliation_queue.slots.lock();
            match slots.entry(name.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(ReconciliationSlot {
                        active_cancellation: None,
                        pending: Some(request),
                    });
                    (true, None)
                }
                Entry::Occupied(mut entry) => {
                    if let Some(active) = &entry.get().active_cancellation {
                        active.cancel();
                    }
                    let superseded = entry.get_mut().pending.replace(request);
                    (false, superseded)
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

        if spawn_worker {
            let reconciler = self.clone();
            let runtime = self.runtime.clone();
            let worker = self.tasks.spawn_on(
                async move {
                    reconciler.run_scheduled(name).await;
                },
                &runtime,
            );
            drop(worker);
        }

        (receiver, superseded)
    }

    /// Cancels active and pending preflight for a task being deleted.
    pub(crate) fn cancel_scheduled(&self, name: &TaskId) {
        let slots = self.reconciliation_queue.slots.lock();
        let Some(slot) = slots.get(name) else {
            return;
        };
        if let Some(active) = &slot.active_cancellation {
            active.cancel();
        }
        if let Some(pending) = &slot.pending {
            pending.cancel_handle.cancel();
        }
    }

    async fn run_scheduled(&self, name: TaskId) {
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
                slot.active_cancellation = Some(request.cancel_handle.clone());
                request
            };

            let result = self
                .reconcile_with_cancellation(
                    request.desired,
                    request.source,
                    request.ensure_output,
                    request.cancel_handle,
                    request.cancellation,
                )
                .await;
            let _ = request.completion.send(result);

            let has_pending = {
                let mut slots = self.reconciliation_queue.slots.lock();
                let slot = slots
                    .get_mut(&name)
                    .expect("active reconciliation slot exists");
                slot.active_cancellation = None;
                if slot.pending.is_some() {
                    true
                } else {
                    slots.remove(&name);
                    false
                }
            };
            if !has_pending {
                return;
            }
        }
    }

    pub(crate) fn spawn_retention_worker(&self, config: StateConfig) {
        let state = self.state.clone();
        let stop = self.retention_stop.clone();
        let interval = config.sweep_interval();
        let start = tokio::time::Instant::now() + interval;
        let runtime = self.runtime.clone();
        let worker = self.tasks.spawn_on(
            async move {
                let mut ticker = tokio::time::interval_at(start, interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = ticker.tick() => {
                            state.sweep(&config);
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
            CoreError::Runner(solti_runner::RouterError::RunIdMismatch { .. }) => {
                "RunnerContractViolation"
            }
            CoreError::Runner(_) => "RunnerBuildFailed",
            CoreError::Mapping(_) => "PolicyMappingFailed",
            CoreError::Supervisor { op: "prepare", .. } => "RuntimePreparationFailed",
            _ => "ReconciliationFailed",
        }
    }

    fn prepare_submission(
        &self,
        task: &Task,
        task_ref: TaskRef,
    ) -> Result<PreparedSubmission, CoreError> {
        let spec = task.spec();
        let task_spec = TvTaskSpec::new(
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
        cancel_handle: &BuildCancellationHandle,
        cancellation: &BuildCancellation,
    ) -> BuildOutcome {
        let admitted = tokio::select! {
            biased;
            _ = self.preflight_stop.cancelled() => {
                cancel_handle.cancel();
                return BuildOutcome::Cancelled;
            }
            admitted = self.router.admit(
                desired,
                &self.build_admission,
                cancellation.clone(),
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

        let deadline = tokio::time::sleep(self.build_timeout);
        tokio::pin!(deadline);
        let mut build = AbortOnDropHandle::new(
            self.runtime
                .spawn(async move { admitted.build().await.map_err(CoreError::from) }),
        );

        let result = tokio::select! {
            biased;
            _ = self.preflight_stop.cancelled() => {
                cancel_handle.cancel();
                build.abort();
                let _ = build.await;
                return BuildOutcome::Cancelled;
            }
            _ = cancellation.cancelled() => {
                build.abort();
                let _ = build.await;
                return BuildOutcome::Cancelled;
            }
            _ = &mut deadline => {
                cancel_handle.cancel();
                build.abort();
                let _ = build.await;
                return BuildOutcome::TimedOut;
            }
            result = &mut build => result,
        };

        match result {
            Ok(Ok(task_ref)) => BuildOutcome::Built(task_ref),
            Ok(Err(error)) => BuildOutcome::Failed(error),
            Err(error) if error.is_panic() => BuildOutcome::Panicked,
            Err(error) => BuildOutcome::Unavailable(error.to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn reconcile(
        &self,
        desired: Task,
        source: RuntimeSource,
        ensure_output: bool,
    ) -> Task {
        let (cancel_handle, cancellation) = BuildCancellation::pair();
        self.reconcile_with_cancellation(
            desired,
            source,
            ensure_output,
            cancel_handle,
            cancellation,
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
            runtime_source = source.as_label(),
        )
    )]
    async fn reconcile_with_cancellation(
        &self,
        desired: Task,
        source: RuntimeSource,
        ensure_output: bool,
        cancel_handle: BuildCancellationHandle,
        cancellation: BuildCancellation,
    ) -> Task {
        let target = ResourceGeneration::from_task(&desired);
        if !self.state.is_current(&target) {
            return self.current(&target, desired);
        }
        if self.preflight_stop.is_cancelled() {
            return self.current(&target, desired);
        }

        let task_ref = match source {
            RuntimeSource::Prebuilt(task_ref) => task_ref,
            RuntimeSource::Routed => match self
                .build_routed(&desired, &cancel_handle, &cancellation)
                .await
            {
                BuildOutcome::Built(task_ref) => task_ref,
                BuildOutcome::Failed(error) => {
                    self.state.mark_reconciliation_failed(
                        &target,
                        Self::failure_reason(&error),
                        error.to_string(),
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
                    self.state.mark_reconciliation_failed(
                        &target,
                        "RunnerBuildPanicked",
                        "reconciliation preflight panicked".to_string(),
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
                    self.state.mark_reconciliation_failed(
                        &target,
                        "RunnerBuildTimedOut",
                        format!(
                            "runner build exceeded {} ms deadline",
                            self.build_timeout.as_millis()
                        ),
                    );
                    return self.current(&target, desired);
                }
                BuildOutcome::Cancelled => {
                    return self.current(&target, desired);
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
                    self.state.mark_reconciliation_failed(
                        &target,
                        "RunnerBuildUnavailable",
                        "reconciliation preflight worker was unavailable".to_string(),
                    );
                    return self.current(&target, desired);
                }
            },
        };

        if self.preflight_stop.is_cancelled() || cancellation.is_cancelled() {
            return self.current(&target, desired);
        }
        let prepared = match catch_unwind(AssertUnwindSafe(|| {
            self.prepare_submission(&desired, task_ref)
        })) {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                self.state.mark_reconciliation_failed(
                    &target,
                    Self::failure_reason(&error),
                    error.to_string(),
                );
                return self.current(&target, desired);
            }
            Err(_) => {
                warn!(
                    event = "task.reconcile_failed",
                    task_name = %target.name,
                    task_uid = %target.uid,
                    generation = target.generation,
                    error_kind = "runner_build_panicked",
                    "runner preflight panicked"
                );
                self.state.mark_reconciliation_failed(
                    &target,
                    "RunnerBuildPanicked",
                    "reconciliation preflight panicked".to_string(),
                );
                return self.current(&target, desired);
            }
        };

        let _runtime_operation = tokio::select! {
            biased;
            _ = self.preflight_stop.cancelled() => {
                return self.current(&target, desired);
            }
            _ = cancellation.cancelled() => {
                return self.current(&target, desired);
            }
            operation = self.runtime_operations.lock(&target.name) => operation,
        };
        if self.preflight_stop.is_cancelled()
            || cancellation.is_cancelled()
            || !self.state.is_current(&target)
        {
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
                    self.state.mark_reconciliation_failed(
                        &target,
                        "PreviousRuntimeCleanupFailed",
                        CoreError::supervisor("cancel", error).to_string(),
                    );
                    return self.current(&target, desired);
                }
            }
        }

        if self.preflight_stop.is_cancelled()
            || cancellation.is_cancelled()
            || !self.state.is_current(&target)
        {
            return self.current(&target, desired);
        }

        let tv = prepared.id();
        if !self.observer.bind(target.clone(), tv, ensure_output) {
            self.state.mark_reconciliation_failed(
                &target,
                "RuntimeBindingFailed",
                "resource changed before runtime binding".to_string(),
            );
            return self.current(&target, desired);
        }
        let binding = RuntimeBinding {
            resource: target.clone(),
            tv,
        };

        match prepared.submit_and_watch().await {
            Ok((submitted, waiter)) => {
                debug_assert_eq!(submitted, tv);
                self.state.mark_observed(&target);
                self.spawn_completion_waiter(binding, waiter);
            }
            Err(error) => {
                self.observer.fail_bound_reconciliation(
                    &binding,
                    "RuntimeSubmissionFailed",
                    CoreError::supervisor("submit", error).to_string(),
                );
            }
        }
        self.current(&target, desired)
    }

    fn current(&self, target: &ResourceGeneration, fallback: Task) -> Task {
        self.state.get_retained(&target.name).unwrap_or(fallback)
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
                                observer.finalize_unavailable(tv_raw, unavailable);
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
