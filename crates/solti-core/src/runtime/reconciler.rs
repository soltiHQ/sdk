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
//! The task tracker lets shutdown wait for every tracked worker.
//! Shutdown stops awaiting runner builds before draining that tracker.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use solti_model::Task;
use solti_runner::RunnerRouter;
use taskvisor::{
    ControllerSpec, PreparedSubmission, SupervisorHandle, TaskRef, TaskSpec as TvTaskSpec,
};
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};
use tracing::{instrument, warn};

use super::{RuntimeObserver, TaskLocks};
use crate::{
    CoreError, StateConfig,
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
}

impl Reconciler {
    pub(crate) fn new(
        output_hub: Arc<OutputHub>,
        handle: SupervisorHandle,
        router: RunnerRouter,
        state: TaskState,
        observer: Arc<RuntimeObserver>,
        grace: Duration,
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
    pub(crate) async fn reconcile(
        &self,
        desired: Task,
        source: RuntimeSource,
        ensure_output: bool,
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
            RuntimeSource::Routed => {
                let router = Arc::clone(&self.router);
                let build_task = desired.clone();
                let mut build = AbortOnDropHandle::new(self.runtime.spawn_blocking(move || {
                    catch_unwind(AssertUnwindSafe(|| {
                        router.build(&build_task).map_err(CoreError::from)
                    }))
                }));
                let build = tokio::select! {
                    biased;
                    _ = self.preflight_stop.cancelled() => {
                        build.abort();
                        return self.current(&target, desired);
                    }
                    result = &mut build => result,
                };
                match build {
                    Ok(Ok(Ok(task_ref))) => task_ref,
                    Ok(Ok(Err(error))) => {
                        self.state.mark_reconciliation_failed(
                            &target,
                            Self::failure_reason(&error),
                            error.to_string(),
                        );
                        return self.current(&target, desired);
                    }
                    Ok(Err(_)) => {
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
                    Err(error) => {
                        warn!(
                            event = "task.reconcile_failed",
                            task_name = %target.name,
                            task_uid = %target.uid,
                            generation = target.generation,
                            error_kind = "runner_build_unavailable",
                            %error,
                            "runner preflight unavailable"
                        );
                        self.state.mark_reconciliation_failed(
                            &target,
                            "RunnerBuildUnavailable",
                            "reconciliation preflight worker was unavailable".to_string(),
                        );
                        return self.current(&target, desired);
                    }
                }
            }
        };

        if self.preflight_stop.is_cancelled() {
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

        let _runtime_operation = self.runtime_operations.lock(&target.name).await;
        if !self.state.is_current(&target) {
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

        if !self.state.is_current(&target) {
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
