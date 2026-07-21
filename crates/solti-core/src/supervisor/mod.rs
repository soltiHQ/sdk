//! Kubernetes-style desired-state supervisor API.
//!
//! [`SupervisorApi`] stores a complete [`Task`] resource first and reconciles
//! that desired state with Taskvisor in an SDK-owned worker. Runtime events are
//! correlated with one exact resource UID and generation.
//!
//! Reconciliation is latest-wins: a stale generation cannot bind or replace the
//! current runtime. The controller does not provide staged-rollout or availability
//! guarantees; side effects already accepted before a generation becomes stale
//! are not rolled back.

use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use solti_model::{
    ConditionStatus, ModelError, Task, TaskId, TaskManifest, TaskPage, TaskPhase, TaskQuery,
    TaskRun, TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::RunnerRouter;
use taskvisor::{
    ControllerConfig, ControllerSpec, PreparedSubmission, Subscribe, Supervisor, SupervisorConfig,
    SupervisorHandle, TaskRef, TaskSpec as TvTaskSpec,
};
use tokio::sync::oneshot;
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};
use tracing::{debug, info, instrument, warn};

use crate::{
    error::CoreError,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    output::{OutputConfig, OutputHub, OutputSubscription},
    state::{
        DesiredCommit, ResourceGeneration, RuntimeBinding, SWEEP_NAME, SWEEP_SLOT, StateConfig,
        StateSubscriber, TaskState, state_sweep,
    },
};

/// High-level API over desired Task resources and the Taskvisor runtime.
pub struct SupervisorApi {
    reconciler: Reconciler,
    task_operations: TaskLocks,
    spawn_gate: parking_lot::Mutex<()>,
    shutdown_started: AtomicBool,
}

/// Cloneable dependencies owned by reconciliation and completion workers.
#[derive(Clone)]
struct Reconciler {
    output_hub: Arc<OutputHub>,
    handle: SupervisorHandle,
    router: Arc<RunnerRouter>,
    state: TaskState,
    state_subscriber: Arc<StateSubscriber>,
    runtime: tokio::runtime::Handle,
    tasks: TaskTracker,
    runtime_operations: TaskLocks,
    grace: Duration,
}

/// Source of the concrete Taskvisor task for one reconciliation.
enum RuntimeSource {
    Routed,
    Prebuilt(TaskRef),
}

#[derive(Clone, Copy)]
enum WriteMode {
    Create,
    Apply,
}

/// Weak keyed async locks for one class of per-resource operations.
///
/// Desired-state and runtime operations deliberately use distinct instances.
#[derive(Clone, Default)]
struct TaskLocks {
    locks: Arc<parking_lot::Mutex<HashMap<TaskId, Weak<tokio::sync::Mutex<()>>>>>,
}

/// Desired-state commit together with an optional first-reconciliation acknowledgement.
struct ScheduledWrite {
    committed: Task,
    reconciliation: Option<oneshot::Receiver<Task>>,
}

impl TaskLocks {
    async fn lock(&self, name: &TaskId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock();
            if let Some(lock) = locks.get(name).and_then(Weak::upgrade) {
                lock
            } else {
                locks.retain(|_, lock| lock.strong_count() > 0);
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(name.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

impl Reconciler {
    fn failure_reason(error: &CoreError) -> &'static str {
        match error {
            CoreError::Runner(_) => "RunnerBuildFailed",
            CoreError::Mapping(_) => "PolicyMappingFailed",
            CoreError::Supervisor { op: "prepare", .. } => "RuntimePreparationFailed",
            _ => "ReconciliationFailed",
        }
    }

    fn preflight(
        &self,
        task: &Task,
        source: RuntimeSource,
    ) -> Result<PreparedSubmission, CoreError> {
        let task_ref = match source {
            RuntimeSource::Routed => self.router.build(task)?,
            RuntimeSource::Prebuilt(task_ref) => task_ref,
        };
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

    async fn reconcile(&self, desired: Task, source: RuntimeSource, ensure_output: bool) -> Task {
        let target = ResourceGeneration::from_task(&desired);
        // Do not perform runner construction for a generation already superseded
        // before this worker starts.
        if !self.state.is_current(&target) {
            return self.current(&target, desired);
        }
        let preflight_reconciler = self.clone();
        let preflight_task = desired.clone();
        let preflight = self
            .runtime
            .spawn_blocking(move || {
                catch_unwind(AssertUnwindSafe(|| {
                    preflight_reconciler.preflight(&preflight_task, source)
                }))
            })
            .await;
        let prepared = match preflight {
            Ok(Ok(Ok(prepared))) => prepared,
            Ok(Ok(Err(error))) => {
                self.state.mark_reconciliation_failed(
                    &target,
                    Self::failure_reason(&error),
                    error.to_string(),
                );
                return self.current(&target, desired);
            }
            Ok(Err(_)) => {
                warn!(task = %target.name, "runner preflight panicked");
                self.state.mark_reconciliation_failed(
                    &target,
                    "RunnerBuildPanicked",
                    "reconciliation preflight panicked".to_string(),
                );
                return self.current(&target, desired);
            }
            Err(error) => {
                warn!(task = %target.name, %error, "runner preflight worker was unavailable");
                self.state.mark_reconciliation_failed(
                    &target,
                    "RunnerBuildUnavailable",
                    "reconciliation preflight worker was unavailable".to_string(),
                );
                return self.current(&target, desired);
            }
        };

        // Runner construction is allowed to overlap later desired commits. All
        // Taskvisor effects for one resource name are serialized after that
        // potentially blocking work, and stale generations stop here without
        // touching the current realization.
        let _runtime_operation = self.runtime_operations.lock(&target.name).await;
        if !self.state.is_current(&target) {
            return self.current(&target, desired);
        }

        // A new desired generation is already committed. The previous runtime is
        // stopped only after the new generation has passed every synchronous
        // preflight step.
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
                    self.state_subscriber
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

        // Desired state may advance while cancellation waits for confirmed
        // cleanup. Never bind a prepared runtime for that stale generation.
        if !self.state.is_current(&target) {
            return self.current(&target, desired);
        }

        let tv = prepared.id();
        if !self
            .state_subscriber
            .bind(target.clone(), tv, ensure_output)
        {
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
                self.state_subscriber.fail_bound_reconciliation(
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
        let subscriber = Arc::clone(&self.state_subscriber);
        let cleanup_handle = self.handle.clone();
        let cleanup_timeout = self.grace.saturating_add(Duration::from_secs(1));
        let tv = binding.tv;
        let tv_raw = tv.get();
        let task = self.tasks.spawn_on(
            async move {
                match waiter.wait().await {
                    Ok(outcome) => subscriber.finalize_from_outcome(tv_raw, &outcome).await,
                    Err(error) => {
                        warn!(
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
                                subscriber
                                    .finalize_unavailable_after_cleanup(tv_raw, unavailable)
                                    .await;
                            }
                            Err(cleanup_error) => {
                                warn!(
                                    taskvisor_id = tv_raw,
                                    error = %cleanup_error,
                                    "could not confirm cleanup after task outcome became unavailable"
                                );
                                subscriber.finalize_unavailable(tv_raw, unavailable);
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

impl Drop for SupervisorApi {
    fn drop(&mut self) {
        let _gate = self.spawn_gate.lock();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.reconciler.tasks.close();
        let handle = self.reconciler.handle.clone();
        let tasks = self.reconciler.tasks.clone();
        let task = self.reconciler.runtime.spawn(async move {
            let _ = handle.shutdown().await;
            tasks.wait().await;
        });
        drop(task);
    }
}

impl SupervisorApi {
    /// Create a supervisor and start its runtime.
    pub async fn new(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
    ) -> Result<Self, CoreError> {
        Self::new_with_output_config(
            sup_cfg,
            ctrl_cfg,
            subscribers,
            router,
            state_cfg,
            OutputConfig::default(),
        )
        .await
    }

    /// Create a supervisor with an explicit live-output buffer configuration.
    pub async fn new_with_output_config(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
        output_config: OutputConfig,
    ) -> Result<Self, CoreError> {
        let output_hub = Arc::new(OutputHub::new(output_config));
        let router = Arc::new(router.with_output_publisher(output_hub.clone()));
        let state = TaskState::new();
        state.set_max_runs_per_task(state_cfg.max_runs_per_task);
        let state_subscriber = Arc::new(StateSubscriber::with_output_hub(
            state.clone(),
            Arc::clone(&output_hub),
        ));
        subscribers.push(state_subscriber.clone());

        let grace = sup_cfg.grace();
        let supervisor = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();
        let handle = supervisor.serve();
        let reconciler = Reconciler {
            output_hub,
            handle,
            router,
            state,
            state_subscriber,
            runtime: tokio::runtime::Handle::current(),
            tasks: TaskTracker::new(),
            runtime_operations: TaskLocks::default(),
            grace,
        };
        let api = Self {
            reconciler,
            task_operations: TaskLocks::default(),
            spawn_gate: parking_lot::Mutex::new(()),
            shutdown_started: AtomicBool::new(false),
        };

        let (task_ref, manifest) = state_sweep(api.reconciler.state.clone(), state_cfg);
        let scheduled = api
            .write(
                manifest,
                RuntimeSource::Prebuilt(task_ref),
                WriteMode::Create,
                false,
            )
            .await?;
        let sweep = api.await_reconciliation(scheduled).await;
        if sweep.status().reconciled().status == ConditionStatus::False {
            warn!(
                reason = %sweep.status().reconciled().reason,
                message = %sweep.status().reconciled().message,
                "internal state sweep reconciliation failed"
            );
        }
        info!("supervisor is ready (state sweep desired resource installed)");
        Ok(api)
    }

    /// Create desired state and schedule reconciliation through the registered runner.
    ///
    /// A successful return confirms the in-memory desired-state commit, not runtime
    /// realization. Routing or runtime failures are recorded later in `status`;
    /// observe `status.conditions[type=Reconciled]` to track reconciliation.
    pub async fn create_task(&self, manifest: TaskManifest) -> Result<Task, CoreError> {
        Self::ensure_public_manifest(&manifest)?;
        Ok(self
            .write(manifest, RuntimeSource::Routed, WriteMode::Create, true)
            .await?
            .committed)
    }

    /// Commit desired state with a caller-supplied Taskvisor task and schedule reconciliation.
    ///
    /// Runtime failures are reported asynchronously through the stored resource's
    /// `Reconciled` condition.
    pub async fn create_with_task(
        &self,
        manifest: TaskManifest,
        task_ref: TaskRef,
    ) -> Result<Task, CoreError> {
        Self::ensure_public_manifest(&manifest)?;
        Ok(self
            .write(
                manifest,
                RuntimeSource::Prebuilt(task_ref),
                WriteMode::Create,
                true,
            )
            .await?
            .committed)
    }

    /// Commit desired state and schedule reconciliation through the registered runner.
    ///
    /// A successful return confirms only the desired-state commit. Routing or
    /// runtime failures are recorded later in the stored resource's `Reconciled`
    /// condition. An identical apply schedules one manual retry only while that
    /// condition is `False`.
    pub async fn apply_task(&self, manifest: TaskManifest) -> Result<Task, CoreError> {
        Self::ensure_public_manifest(&manifest)?;
        Ok(self
            .write(manifest, RuntimeSource::Routed, WriteMode::Apply, true)
            .await?
            .committed)
    }

    /// Apply desired state unless an existing resource is hidden by an adapter-owned policy.
    ///
    /// An absent resource is created normally. The visibility check and desired
    /// commit share the same per-name lock, so an apply cannot replace a hidden
    /// incarnation through a check-then-write race.
    pub async fn apply_task_where<F>(
        &self,
        manifest: TaskManifest,
        predicate: F,
    ) -> Result<Task, CoreError>
    where
        F: Fn(&Task) -> bool,
    {
        Self::ensure_public_manifest(&manifest)?;
        let name = manifest.name().clone();
        let operation = self.task_operations.lock(&name).await;
        if self
            .reconciler
            .state
            .get_retained(&name)
            .is_some_and(|task| !predicate(&task))
        {
            return Err(CoreError::NotFound(name.to_string()));
        }
        Ok(self
            .write_locked(
                manifest,
                RuntimeSource::Routed,
                WriteMode::Apply,
                true,
                operation,
            )?
            .committed)
    }

    /// Commit desired state with a caller-supplied Taskvisor task and schedule reconciliation.
    ///
    /// Runtime failures are reported asynchronously through the stored resource's
    /// `Reconciled` condition. An identical apply schedules one manual retry only
    /// while that condition is `False`.
    pub async fn apply_with_task(
        &self,
        manifest: TaskManifest,
        task_ref: TaskRef,
    ) -> Result<Task, CoreError> {
        Self::ensure_public_manifest(&manifest)?;
        Ok(self
            .write(
                manifest,
                RuntimeSource::Prebuilt(task_ref),
                WriteMode::Apply,
                true,
            )
            .await?
            .committed)
    }

    fn ensure_public_manifest(manifest: &TaskManifest) -> Result<(), CoreError> {
        Self::ensure_public_name(manifest.name())?;
        if manifest.slot().as_str() == SWEEP_SLOT {
            return Err(CoreError::ReservedResource(format!(
                "spec.slot '{}' is owned by solti-core",
                SWEEP_SLOT
            )));
        }
        Ok(())
    }

    fn ensure_public_name(name: &TaskId) -> Result<(), CoreError> {
        if name.as_str() == SWEEP_NAME {
            return Err(CoreError::ReservedResource(format!(
                "metadata.name '{}' is owned by solti-core",
                SWEEP_NAME
            )));
        }
        Ok(())
    }

    async fn write(
        &self,
        manifest: TaskManifest,
        source: RuntimeSource,
        mode: WriteMode,
        ensure_output: bool,
    ) -> Result<ScheduledWrite, CoreError> {
        Self::ensure_runtime_contract(&manifest, &source)?;
        // The stable resource address is metadata.name. TaskRef::name is only a
        // runtime diagnostic label and is deliberately not consulted here.
        let name = manifest.name().clone();
        let operation = self.task_operations.lock(&name).await;
        self.write_locked(manifest, source, mode, ensure_output, operation)
    }

    fn write_locked(
        &self,
        manifest: TaskManifest,
        source: RuntimeSource,
        mode: WriteMode,
        ensure_output: bool,
        _operation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ScheduledWrite, CoreError> {
        Self::ensure_runtime_contract(&manifest, &source)?;
        let _spawn = self.spawn_gate.lock();
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::ShuttingDown);
        }

        // Keep the tracker non-empty from desired-state commit through the
        // tracked worker spawn. TaskTracker::close does not reject spawns.
        let registration = self.reconciler.tasks.token();
        let commit = match mode {
            WriteMode::Create => self.reconciler.state.create_desired(&manifest)?,
            WriteMode::Apply => self.reconciler.state.apply_desired(&manifest)?,
        };
        if !commit.reconcile {
            drop(registration);
            return Ok(ScheduledWrite {
                committed: commit.task,
                reconciliation: None,
            });
        }

        let committed = commit.task.clone();
        let reconciliation = self.spawn_reconciliation(commit, source, ensure_output, registration);
        Ok(ScheduledWrite {
            committed,
            reconciliation: Some(reconciliation),
        })
    }

    async fn await_reconciliation(&self, scheduled: ScheduledWrite) -> Task {
        let name = scheduled.committed.name().clone();
        let fallback = scheduled.committed;
        match scheduled.reconciliation {
            Some(receiver) => receiver.await.unwrap_or_else(|_| {
                self.reconciler
                    .state
                    .get_retained(&name)
                    .unwrap_or(fallback)
            }),
            None => fallback,
        }
    }

    fn ensure_runtime_contract(
        manifest: &TaskManifest,
        source: &RuntimeSource,
    ) -> Result<(), CoreError> {
        let embedded = matches!(manifest.spec().workload(), TaskWorkload::Embedded(_));
        match (source, embedded) {
            (RuntimeSource::Prebuilt(_), true) | (RuntimeSource::Routed, false) => Ok(()),
            (RuntimeSource::Prebuilt(_), false) => {
                Err(CoreError::InvalidSpec(ModelError::Invalid(
                    "a caller-supplied TaskRef requires spec.workload kind Embedded".into(),
                )))
            }
            (RuntimeSource::Routed, true) => Err(CoreError::InvalidSpec(ModelError::Invalid(
                "spec.workload kind Embedded requires create_with_task() or apply_with_task()"
                    .into(),
            ))),
        }
    }

    fn spawn_reconciliation(
        &self,
        commit: DesiredCommit,
        source: RuntimeSource,
        ensure_output: bool,
        registration: TaskTrackerToken,
    ) -> oneshot::Receiver<Task> {
        let reconciler = self.reconciler.clone();
        let runtime = reconciler.runtime.clone();
        let (sender, receiver) = oneshot::channel();
        let worker = self.reconciler.tasks.spawn_on(
            async move {
                let _registration = registration;
                let task = reconciler
                    .reconcile(commit.task, source, ensure_output)
                    .await;
                let _ = sender.send(task);
            },
            &runtime,
        );
        drop(worker);
        receiver
    }

    /// Subscribe to one resource's lossy live output stream.
    pub fn subscribe_output(&self, name: &TaskId) -> Option<OutputSubscription> {
        if name.as_str() == SWEEP_NAME {
            return None;
        }
        self.reconciler.output_hub.subscribe(name)
    }

    /// Subscribe only when the current Task satisfies an adapter-owned predicate.
    ///
    /// The predicate, current binding and output subscription are captured under
    /// the desired-state and runtime per-name locks. The returned generation lets
    /// the adapter discard any queued event from a different desired generation.
    pub async fn subscribe_output_where<F>(
        &self,
        name: &TaskId,
        predicate: F,
    ) -> Option<(u64, OutputSubscription)>
    where
        F: Fn(&Task) -> bool,
    {
        if name.as_str() == SWEEP_NAME {
            return None;
        }
        let _operation = self.task_operations.lock(name).await;
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        let task = self.reconciler.state.get_retained(name)?;
        if !predicate(&task) {
            return None;
        }
        let resource = ResourceGeneration::from_task(&task);
        if self.reconciler.state.binding_for(name)?.resource != resource {
            return None;
        }
        let subscription = self.reconciler.output_hub.subscribe(name)?;
        Some((resource.generation, subscription))
    }

    /// Return one retained Task resource by metadata.name.
    pub fn get_task(&self, name: &TaskId) -> Option<Task> {
        self.reconciler.state.get(name)
    }

    /// List every retained public Task in one slot.
    pub fn list_tasks_by_slot(&self, slot: &str) -> Vec<Task> {
        self.reconciler.state.list_by_slot(slot)
    }

    /// List all retained public Tasks. Core's internal sweep resource is excluded.
    pub fn list_all_tasks(&self) -> Vec<Task> {
        self.reconciler.state.list_all()
    }

    /// List retained public Tasks in one phase.
    pub fn list_tasks_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        self.reconciler.state.list_by_status(phase)
    }

    /// Query retained public Tasks with filters and pagination.
    pub fn query_tasks(&self, query: &TaskQuery) -> TaskPage<Task> {
        self.reconciler.state.query(query)
    }

    /// Query Tasks with an adapter-owned predicate applied before pagination.
    ///
    /// Core itself does not attach wire-visibility semantics to workloads. A
    /// transport can use this method to hide unsupported workloads while still
    /// receiving a correct `total`, offset and limit.
    pub fn query_tasks_where<F>(&self, query: &TaskQuery, predicate: F) -> TaskPage<Task>
    where
        F: Fn(&Task) -> bool,
    {
        self.reconciler.state.query_where(query, predicate)
    }

    /// List execution history for one resource, oldest generation and attempt first.
    pub fn list_task_runs(&self, name: &TaskId) -> Vec<TaskRun> {
        self.reconciler.state.list_runs(name)
    }

    /// List runs visible under an adapter-owned workload-GVK predicate.
    ///
    /// Visibility is checked under the same per-name operation lock used by
    /// apply and delete, so an apply cannot change the workload between the
    /// check and the run snapshot.
    pub async fn list_task_runs_where<F>(&self, name: &TaskId, predicate: F) -> Option<Vec<TaskRun>>
    where
        F: Fn(&WorkloadTypeMeta) -> bool,
    {
        if name.as_str() == SWEEP_NAME {
            return None;
        }
        let _operation = self.task_operations.lock(name).await;
        let task = self.reconciler.state.get_retained(name)?;
        if !predicate(&task.spec().workload().type_meta()) {
            return None;
        }
        Some(
            self.reconciler
                .state
                .list_runs(name)
                .into_iter()
                .filter(|run| predicate(&run.workload))
                .collect(),
        )
    }

    /// Return a shared read handle for the in-memory resource store.
    pub fn state(&self) -> TaskState {
        self.reconciler.state.clone()
    }

    async fn cancel_bound(
        &self,
        name: &TaskId,
    ) -> Result<Option<(RuntimeBinding, bool)>, CoreError> {
        let Some(binding) = self.reconciler.state.binding_for(name) else {
            return Ok(None);
        };
        let claimed = self
            .reconciler
            .handle
            .cancel_with_timeout(
                binding.tv,
                self.reconciler.grace.saturating_add(Duration::from_secs(1)),
            )
            .await
            .map_err(|error| CoreError::supervisor("cancel", error))?;
        Ok(Some((binding, claimed)))
    }

    /// Cancel the currently bound or queued Taskvisor realization while retaining desired state.
    ///
    /// Before reconciliation creates a binding this is a no-op; it does not
    /// create a hidden intent that suppresses later reconciliation.
    #[instrument(level = "debug", skip(self), fields(task = %name))]
    pub async fn cancel_task(&self, name: &TaskId) -> Result<(), CoreError> {
        Self::ensure_public_name(name)?;
        let _operation = self.task_operations.lock(name).await;
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        let was_known = self.reconciler.state.contains_task(name);
        let cancellation = self.cancel_bound(name).await?;
        let claimed = cancellation.as_ref().is_some_and(|(_, claimed)| *claimed);
        if let Some((binding, _)) = cancellation {
            self.reconciler
                .state_subscriber
                .settle_after_confirmed_cleanup(binding.tv)
                .await;
        }
        if !claimed && !was_known {
            return Err(CoreError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Delete a Task resource and its run history after stopping its realization.
    /// Deleting a missing resource is an idempotent no-op.
    #[instrument(level = "debug", skip(self), fields(task = %name))]
    pub async fn delete_task(&self, name: &TaskId) -> Result<(), CoreError> {
        Self::ensure_public_name(name)?;
        let _operation = self.task_operations.lock(name).await;
        self.delete_task_locked(name).await
    }

    /// Delete only when the current Task satisfies an adapter-owned predicate.
    ///
    /// A missing resource or one rejected by the predicate is reported as absent.
    /// The unconditional [`delete_task`](Self::delete_task) remains idempotent.
    pub async fn delete_task_where<F>(&self, name: &TaskId, predicate: F) -> Result<(), CoreError>
    where
        F: Fn(&Task) -> bool,
    {
        Self::ensure_public_name(name)?;
        let _operation = self.task_operations.lock(name).await;
        let Some(task) = self.reconciler.state.get_retained(name) else {
            return Err(CoreError::NotFound(name.to_string()));
        };
        if !predicate(&task) {
            return Err(CoreError::NotFound(name.to_string()));
        }
        self.delete_task_locked(name).await
    }

    async fn delete_task_locked(&self, name: &TaskId) -> Result<(), CoreError> {
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        debug!(task = %name, "deleting task resource");
        let cancellation = self.cancel_bound(name).await?;
        let tv = cancellation.as_ref().map(|(binding, _)| binding.tv);
        self.reconciler
            .state_subscriber
            .delete_after_cleanup(name, tv);
        Ok(())
    }

    /// Stop Taskvisor and wait for every SDK-owned reconciliation/completion worker.
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        info!("initiating graceful shutdown");
        {
            let _spawn = self.spawn_gate.lock();
            if !self.shutdown_started.swap(true, Ordering::AcqRel) {
                self.reconciler.tasks.close();
            }
        }
        let result = self
            .reconciler
            .handle
            .clone()
            .shutdown()
            .await
            .map_err(|error| CoreError::supervisor("shutdown", error));
        self.reconciler.tasks.wait().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use parking_lot::{Condvar, Mutex};
    use solti_model::{
        AdmissionPolicy, EmbeddedSpec, Flag, Labels, SubprocessMode, SubprocessSpec, TaskEnv,
        TaskSpec, TaskWorkload,
    };
    use solti_runner::{BuildContext, Runner, RunnerError};
    use taskvisor::{TaskContext, TaskError, TaskFn};

    use super::*;

    fn embedded_with_revision(name: &str, timeout_ms: u64, revision: &str) -> TaskManifest {
        TaskManifest::new(
            name,
            TaskSpec::builder(
                "embedded-slot",
                TaskWorkload::Embedded(EmbeddedSpec::new(revision).unwrap()),
                timeout_ms,
            )
            .build()
            .unwrap(),
        )
        .unwrap()
    }

    fn embedded(name: &str, timeout_ms: u64) -> TaskManifest {
        embedded_with_revision(name, timeout_ms, "test-v1")
    }

    fn routed(name: &str, timeout_ms: u64) -> TaskManifest {
        let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: vec![],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        ));
        TaskManifest::new(
            name,
            TaskSpec::builder("routed-slot", workload, timeout_ms)
                .build()
                .unwrap(),
        )
        .unwrap()
    }

    fn reserved_slot(name: &str) -> TaskManifest {
        TaskManifest::new(
            name,
            TaskSpec::builder(
                SWEEP_SLOT,
                TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
                1_000_u64,
            )
            .admission(AdmissionPolicy::Replace)
            .build()
            .unwrap(),
        )
        .unwrap()
    }

    fn immediate_task(name: &str) -> TaskRef {
        TaskFn::arc(
            name,
            |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) },
        )
    }

    fn cancellable_task(name: &str) -> TaskRef {
        TaskFn::arc(name, |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err::<(), TaskError>(TaskError::Canceled)
        })
    }

    async fn api(router: RunnerRouter) -> SupervisorApi {
        SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .unwrap()
    }

    async fn wait_for_task(
        api: &SupervisorApi,
        name: &TaskId,
        predicate: impl Fn(&Task) -> bool,
    ) -> Task {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(task) = api.get_task(name)
                    && predicate(&task)
                {
                    return task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task state did not converge")
    }

    async fn wait_for_observed(api: &SupervisorApi, name: &TaskId, generation: u64) -> Task {
        wait_for_task(api, name, |task| {
            task.status().observed_generation == generation
        })
        .await
    }

    async fn wait_for_reconciled(
        api: &SupervisorApi,
        name: &TaskId,
        generation: u64,
        status: ConditionStatus,
    ) -> Task {
        wait_for_task(api, name, |task| {
            let condition = task.status().reconciled();
            condition.observed_generation == generation && condition.status == status
        })
        .await
    }

    async fn wait_for_binding(
        api: &SupervisorApi,
        name: &TaskId,
        generation: u64,
    ) -> RuntimeBinding {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(binding) = api.reconciler.state.binding_for(name)
                    && binding.resource.generation == generation
                {
                    return binding;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime binding did not converge")
    }

    struct RecordingRunner {
        seen: Arc<Mutex<Vec<(TaskId, u64, String)>>>,
    }

    impl Runner for RecordingRunner {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
            self.seen.lock().push((
                task.name().clone(),
                task.metadata().generation(),
                task.metadata().resource_version().to_string(),
            ));
            Ok(immediate_task("runner-owned-runtime-name"))
        }
    }

    #[tokio::test]
    async fn stale_generation_is_rejected_before_runner_build() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(RecordingRunner {
            seen: Arc::clone(&seen),
        }));
        let api = api(router).await;
        let stale = api
            .reconciler
            .state
            .create_desired(&routed("stale-before-build", 1_000))
            .unwrap()
            .task;
        let current = api
            .reconciler
            .state
            .apply_desired(&routed("stale-before-build", 2_000))
            .unwrap()
            .task;

        let returned = api
            .reconciler
            .reconcile(stale, RuntimeSource::Routed, true)
            .await;

        assert_eq!(returned, current);
        assert!(seen.lock().is_empty());
        assert!(
            api.reconciler
                .state
                .binding_for(&TaskId::from("stale-before-build"))
                .is_none()
        );
        api.reconciler
            .state
            .delete_task(&TaskId::from("stale-before-build"));
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn all_four_resource_write_paths_accept_desired_manifests() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(RecordingRunner {
            seen: Arc::clone(&seen),
        }));
        let api = api(router).await;

        let created = api
            .create_task(routed("routed-resource", 1_000))
            .await
            .unwrap();
        assert_eq!(created.name().as_str(), "routed-resource");
        assert!(!created.metadata().resource_version().is_empty());
        assert_eq!(created.status().phase, TaskPhase::Pending);
        assert_eq!(created.status().observed_generation, 0);
        wait_for_observed(&api, created.name(), 1).await;

        let mut labels = Labels::new();
        labels.insert("team", "platform");
        let metadata_apply = TaskManifest::new("routed-resource", created.spec().clone())
            .unwrap()
            .with_labels(labels.clone());
        let applied = api.apply_task(metadata_apply).await.unwrap();
        assert_eq!(applied.metadata().generation(), 1);
        assert_eq!(applied.metadata().labels(), &labels);

        let applied = api
            .apply_task(routed("routed-resource", 2_000))
            .await
            .unwrap();
        assert_eq!(applied.metadata().generation(), 2);
        assert_eq!(applied.status().phase, TaskPhase::Pending);
        assert_eq!(applied.status().observed_generation, 1);
        wait_for_observed(&api, applied.name(), 2).await;

        let embedded_created = api
            .create_with_task(
                embedded("embedded-resource", 1_000),
                immediate_task("unrelated-runtime-name"),
            )
            .await
            .unwrap();
        assert_eq!(embedded_created.name().as_str(), "embedded-resource");
        assert_eq!(embedded_created.status().phase, TaskPhase::Pending);
        wait_for_observed(&api, embedded_created.name(), 1).await;
        assert!(
            api.get_task(&TaskId::from("unrelated-runtime-name"))
                .is_none()
        );

        let embedded_applied = api
            .apply_with_task(
                embedded("embedded-resource", 2_000),
                immediate_task("another-runtime-name"),
            )
            .await
            .unwrap();
        assert_eq!(embedded_applied.metadata().generation(), 2);
        assert_eq!(embedded_applied.status().phase, TaskPhase::Pending);
        wait_for_observed(&api, embedded_applied.name(), 2).await;

        {
            let seen = seen.lock();
            assert_eq!(seen.len(), 2, "metadata-only apply must not rebuild");
            assert_eq!(seen[0].0.as_str(), "routed-resource");
            assert_eq!(seen[0].1, 1);
            assert!(!seen[0].2.is_empty(), "runner receives the stored Task");
            assert_eq!(seen[1].1, 2);
        }

        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn embedded_revision_controls_reconciliation_generation() {
        let api = api(RunnerRouter::new()).await;
        let first = api
            .create_with_task(
                embedded_with_revision("embedded-revision", 10_000, "v1"),
                cancellable_task("runtime-v1"),
            )
            .await
            .unwrap();
        let first_binding = wait_for_binding(&api, first.name(), 1).await;

        let unchanged = api
            .apply_with_task(
                embedded_with_revision("embedded-revision", 10_000, "v1"),
                cancellable_task("unused-runtime"),
            )
            .await
            .unwrap();
        assert_eq!(unchanged.metadata().generation(), 1);
        assert_eq!(
            api.reconciler.state.binding_for(first.name()),
            Some(first_binding.clone()),
            "an unchanged manifest must not replace its runtime"
        );

        let changed = api
            .apply_with_task(
                embedded_with_revision("embedded-revision", 10_000, "v2"),
                cancellable_task("runtime-v2"),
            )
            .await
            .unwrap();
        assert_eq!(changed.metadata().generation(), 2);
        assert_eq!(changed.status().phase, TaskPhase::Pending);
        let changed_binding = wait_for_binding(&api, changed.name(), 2).await;
        assert_ne!(
            changed_binding, first_binding,
            "a spec generation must receive a distinct runtime binding"
        );

        api.delete_task(changed.name()).await.unwrap();
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn runtime_source_must_match_the_declared_workload_before_commit() {
        let api = api(RunnerRouter::new()).await;

        let prebuilt_routed = api
            .create_with_task(
                routed("prebuilt-routed", 1_000),
                immediate_task("arbitrary-runtime"),
            )
            .await;
        assert!(matches!(prebuilt_routed, Err(CoreError::InvalidSpec(_))));
        assert!(api.get_task(&TaskId::from("prebuilt-routed")).is_none());

        let routed_embedded = api.create_task(embedded("routed-embedded", 1_000)).await;
        assert!(matches!(routed_embedded, Err(CoreError::InvalidSpec(_))));
        assert!(api.get_task(&TaskId::from("routed-embedded")).is_none());

        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sweep_name_and_slot_are_reserved_from_every_public_operation() {
        let api = api(RunnerRouter::new()).await;
        let sweep_name = TaskId::from(SWEEP_NAME);
        let binding = api
            .reconciler
            .state
            .binding_for(&sweep_name)
            .expect("private bootstrap installs the sweep runtime");

        assert!(api.get_task(&sweep_name).is_none());
        assert!(api.state().get(&sweep_name).is_none());
        assert!(api.list_task_runs(&sweep_name).is_empty());
        assert!(api.subscribe_output(&sweep_name).is_none());
        assert!(api.list_tasks_by_slot(SWEEP_SLOT).is_empty());
        assert!(
            api.query_tasks(&TaskQuery::new())
                .items
                .iter()
                .all(|task| task.name() != &sweep_name)
        );

        let mutations = [
            api.create_task(embedded(SWEEP_NAME, 1_000)).await,
            api.apply_task(reserved_slot("slot-intruder")).await,
            api.create_with_task(
                embedded(SWEEP_NAME, 1_000),
                immediate_task("reserved-create"),
            )
            .await,
            api.apply_with_task(
                reserved_slot("slot-intruder-embedded"),
                immediate_task("reserved-apply"),
            )
            .await,
        ];
        assert!(
            mutations
                .into_iter()
                .all(|result| matches!(result, Err(CoreError::ReservedResource(_))))
        );
        assert!(matches!(
            api.delete_task(&sweep_name).await,
            Err(CoreError::ReservedResource(_))
        ));
        assert!(matches!(
            api.cancel_task(&sweep_name).await,
            Err(CoreError::ReservedResource(_))
        ));

        assert_eq!(
            api.reconciler.state.binding_for(&sweep_name),
            Some(binding),
            "rejected public operations cannot stop the maintenance runtime"
        );
        assert!(api.reconciler.state.get_retained(&sweep_name).is_some());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn conditional_reads_and_delete_share_the_resource_operation_lock() {
        let api = api(RunnerRouter::new()).await;
        let task = api
            .create_with_task(
                embedded("conditional", 10_000),
                cancellable_task("conditional-runtime"),
            )
            .await
            .unwrap();
        wait_for_binding(&api, task.name(), task.metadata().generation()).await;

        assert!(
            api.list_task_runs_where(task.name(), |_| false)
                .await
                .is_none()
        );
        assert!(
            api.list_task_runs_where(task.name(), |_| true)
                .await
                .is_some()
        );
        assert!(
            api.subscribe_output_where(task.name(), |_| false)
                .await
                .is_none()
        );
        let (generation, _subscription) = api
            .subscribe_output_where(task.name(), |_| true)
            .await
            .expect("current bound generation has an output channel");
        assert_eq!(generation, task.metadata().generation());

        assert!(matches!(
            api.delete_task_where(task.name(), |_| false).await,
            Err(CoreError::NotFound(_))
        ));
        assert!(api.get_task(task.name()).is_some());
        assert!(matches!(
            api.delete_task_where(&TaskId::from("missing"), |_| true)
                .await,
            Err(CoreError::NotFound(_))
        ));
        api.delete_task_where(task.name(), |_| true).await.unwrap();
        assert!(api.get_task(task.name()).is_none());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn conditional_run_listing_filters_each_historical_workload_snapshot() {
        let api = api(RunnerRouter::new()).await;
        let state = &api.reconciler.state;
        let first = state
            .create_desired(&embedded("run-visibility", 1_000))
            .unwrap()
            .task;
        let old_resource = ResourceGeneration::from_task(&first);
        let old_tv = taskvisor::TaskId::for_tests();
        assert!(state.bind_tv(old_resource.clone(), old_tv));
        let old_binding = RuntimeBinding {
            resource: old_resource,
            tv: old_tv,
        };
        assert!(state.transition_attempt_finished(
            &old_binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));

        let current = state
            .apply_desired(&routed("run-visibility", 1_000))
            .unwrap()
            .task;
        let current_resource = ResourceGeneration::from_task(&current);
        let current_tv = taskvisor::TaskId::for_tests();
        assert!(state.bind_tv(current_resource.clone(), current_tv));
        assert!(state.transition_attempt_starting(
            &RuntimeBinding {
                resource: current_resource,
                tv: current_tv,
            },
            1,
        ));

        let visible = api
            .list_task_runs_where(current.name(), |gvk| gvk.kind() != "Embedded")
            .await
            .expect("the current parent is visible");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].generation, 2);
        assert_eq!(visible[0].workload.kind(), "Subprocess");

        state.delete_task(current.name());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn conditional_apply_cannot_replace_a_hidden_existing_resource() {
        let api = api(RunnerRouter::new()).await;
        let embedded = api
            .create_with_task(
                embedded("hidden-apply", 10_000),
                cancellable_task("hidden-runtime"),
            )
            .await
            .unwrap();

        let result = api
            .apply_task_where(routed("hidden-apply", 1_000), |current| {
                !matches!(current.spec().workload(), TaskWorkload::Embedded(_))
            })
            .await;

        assert!(matches!(result, Err(CoreError::NotFound(_))));
        assert_eq!(api.get_task(embedded.name()), Some(embedded.clone()));

        let created = api
            .apply_task_where(routed("new-visible", 1_000), |_| {
                panic!("predicate must not run for an absent resource")
            })
            .await
            .unwrap();
        assert_eq!(created.name().as_str(), "new-visible");

        api.delete_task(embedded.name()).await.unwrap();
        api.delete_task(created.name()).await.unwrap();
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn preflight_failure_is_retained_in_reconciled_condition() {
        let api = api(RunnerRouter::new()).await;

        let task = api
            .create_task(routed("no-runner", 1_000))
            .await
            .expect("valid desired state is retained");

        assert_eq!(task.status().phase, TaskPhase::Pending);
        assert_eq!(task.status().observed_generation, 0);
        let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().phase, TaskPhase::Pending);
        assert_eq!(failed.status().attempt, 0);
        assert!(failed.status().error.is_none());
        assert_eq!(failed.status().reconciled().reason, "RunnerBuildFailed");
        assert!(
            failed
                .status()
                .reconciled()
                .message
                .contains("no suitable runner")
        );
        assert_eq!(api.get_task(task.name()), Some(failed));
        api.shutdown().await.unwrap();
    }

    struct PanicRunner;

    impl Runner for PanicRunner {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
            panic!("runner build panic")
        }
    }

    #[tokio::test]
    async fn runner_panic_is_contained_as_reconciliation_failure() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(PanicRunner));
        let api = api(router).await;

        let task = api
            .create_task(routed("panic-contained", 1_000))
            .await
            .expect("desired state remains queryable");

        assert_eq!(task.status().phase, TaskPhase::Pending);
        assert_eq!(task.status().observed_generation, 0);
        let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().phase, TaskPhase::Pending);
        assert_eq!(failed.status().attempt, 0);
        assert!(failed.status().error.is_none());
        assert_eq!(failed.status().reconciled().reason, "RunnerBuildPanicked");
        assert_eq!(
            failed.status().reconciled().message,
            "reconciliation preflight panicked"
        );
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_new_generation_does_not_cancel_the_old_runtime() {
        let api = api(RunnerRouter::new()).await;
        let first = api
            .create_with_task(embedded("upgrade", 10_000), cancellable_task("old-runtime"))
            .await
            .unwrap();
        let previous = wait_for_binding(&api, first.name(), 1).await;

        let failed = api.apply_task(routed("upgrade", 2_000)).await.unwrap();
        assert_eq!(failed.metadata().generation(), 2);
        assert_eq!(failed.status().phase, TaskPhase::Pending);
        assert_eq!(failed.status().observed_generation, 1);
        let failed = wait_for_reconciled(&api, failed.name(), 2, ConditionStatus::False).await;
        assert_eq!(failed.status().phase, TaskPhase::Pending);
        assert_eq!(failed.status().reconciled().reason, "RunnerBuildFailed");
        assert_eq!(
            api.reconciler.state.binding_for(first.name()),
            Some(previous),
            "preflight runs before cancellation"
        );

        api.delete_task(failed.name()).await.unwrap();
        api.shutdown().await.unwrap();
    }

    struct BuildGate {
        started: AtomicBool,
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl BuildGate {
        fn new() -> Self {
            Self {
                started: AtomicBool::new(false),
                open: Mutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn release(&self) {
            *self.open.lock() = true;
            self.changed.notify_all();
        }
    }

    struct FailOnceBlockingRunner {
        builds: Arc<AtomicUsize>,
        retry_gate: Arc<BuildGate>,
    }

    impl Runner for FailOnceBlockingRunner {
        fn name(&self) -> &'static str {
            "fail-once-blocking"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
            let build = self.builds.fetch_add(1, Ordering::AcqRel);
            if build == 0 {
                return Err(RunnerError::Internal("transient build failure".into()));
            }
            if build == 1 {
                self.retry_gate.started.store(true, Ordering::Release);
                let mut open = self.retry_gate.open.lock();
                while !*open {
                    self.retry_gate.changed.wait(&mut open);
                }
            }
            Ok(immediate_task("retried-runtime"))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_apply_retries_once_only_while_reconciled_is_false() {
        let builds = Arc::new(AtomicUsize::new(0));
        let retry_gate = Arc::new(BuildGate::new());
        let mut router = RunnerRouter::new();
        router.register(Arc::new(FailOnceBlockingRunner {
            builds: Arc::clone(&builds),
            retry_gate: Arc::clone(&retry_gate),
        }));
        let api = api(router).await;
        let manifest = routed("manual-retry", 1_000);

        let created = api.create_task(manifest.clone()).await.unwrap();
        let failed = wait_for_reconciled(&api, created.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().reconciled().reason, "RunnerBuildFailed");

        let retry = api.apply_task(manifest.clone()).await.unwrap();
        assert_eq!(retry.metadata().generation(), 1);
        assert_eq!(retry.status().reconciled().status, ConditionStatus::Unknown);
        wait_for_build(&retry_gate).await;

        let duplicate = api.apply_task(manifest).await.unwrap();
        assert_eq!(duplicate.metadata().generation(), 1);
        assert_eq!(duplicate, retry);
        assert_eq!(builds.load(Ordering::Acquire), 2);

        retry_gate.release();
        let reconciled = wait_for_reconciled(&api, created.name(), 1, ConditionStatus::True).await;
        assert_eq!(reconciled.status().phase, TaskPhase::Pending);
        assert_eq!(builds.load(Ordering::Acquire), 2);
        api.shutdown().await.unwrap();
    }

    async fn wait_for_build(gate: &BuildGate) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !gate.started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconciliation worker did not reach runner build");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conditional_delete_cannot_delete_a_generation_applied_after_its_predicate() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(RecordingRunner { seen }));
        let api = Arc::new(api(router).await);
        let first = api
            .create_task(routed("visibility-race", 1_000))
            .await
            .unwrap();
        let first_uid = first.uid().clone();
        let name = first.name().clone();
        let gate = Arc::new(BuildGate::new());

        let deletion = {
            let api = Arc::clone(&api);
            let name = name.clone();
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                api.delete_task_where(&name, move |task| {
                    assert!(matches!(
                        task.spec().workload(),
                        TaskWorkload::Subprocess(_)
                    ));
                    gate.started.store(true, Ordering::Release);
                    let mut open = gate.open.lock();
                    while !*open {
                        gate.changed.wait(&mut open);
                    }
                    true
                })
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !gate.started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delete predicate started");

        let apply = {
            let api = Arc::clone(&api);
            let name = name.clone();
            tokio::spawn(async move {
                api.apply_with_task(
                    embedded(name.as_str(), 2_000),
                    immediate_task("hidden-runtime"),
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !apply.is_finished(),
            "apply must wait while delete owns the predicate snapshot"
        );

        gate.release();
        deletion.await.unwrap().unwrap();
        let replacement = apply.await.unwrap().unwrap();
        assert_ne!(replacement.uid(), &first_uid);
        assert!(matches!(
            replacement.spec().workload(),
            TaskWorkload::Embedded(_)
        ));
        assert_eq!(api.get_task(&name), Some(replacement));
        api.shutdown().await.unwrap();
    }

    struct BlockingRunner {
        gate: Arc<BuildGate>,
        runtime_started: Arc<AtomicBool>,
    }

    impl Runner for BlockingRunner {
        fn name(&self) -> &'static str {
            "blocking"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
            self.gate.started.store(true, Ordering::Release);
            let mut open = self.gate.open.lock();
            while !*open {
                self.gate.changed.wait(&mut open);
            }
            let runtime_started = Arc::clone(&self.runtime_started);
            Ok(TaskFn::arc(
                "blocked-build-runtime",
                move |_ctx: TaskContext| {
                    runtime_started.store(true, Ordering::Release);
                    async move { Ok::<(), TaskError>(()) }
                },
            ))
        }
    }

    #[tokio::test]
    async fn desired_commit_returns_before_blocked_reconciliation() {
        let gate = Arc::new(BuildGate::new());
        let runtime_started = Arc::new(AtomicBool::new(false));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(BlockingRunner {
            gate: Arc::clone(&gate),
            runtime_started: Arc::clone(&runtime_started),
        }));
        let api = api(router).await;
        let name = TaskId::from("detached-request");

        let committed = tokio::time::timeout(
            Duration::from_millis(250),
            api.create_task(routed("detached-request", 1_000)),
        )
        .await
        .expect("desired commit must not wait for runner build")
        .unwrap();
        assert_eq!(committed.status().phase, TaskPhase::Pending);
        assert_eq!(committed.status().observed_generation, 0);
        assert_eq!(api.get_task(&name), Some(committed));

        wait_for_build(&gate).await;
        assert!(!runtime_started.load(Ordering::Acquire));
        gate.release();
        wait_for_observed(&api, &name, 1).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !runtime_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("submitted runtime did not start");
        api.shutdown().await.unwrap();
    }

    struct FirstBuildBlockingRunner {
        gate: Arc<BuildGate>,
        builds: AtomicUsize,
    }

    impl Runner for FirstBuildBlockingRunner {
        fn name(&self) -> &'static str {
            "first-build-blocking"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
                self.gate.started.store(true, Ordering::Release);
                let mut open = self.gate.open.lock();
                while !*open {
                    self.gate.changed.wait(&mut open);
                }
            }
            Ok(cancellable_task(&format!(
                "runtime-generation-{}",
                task.metadata().generation()
            )))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn newer_apply_reconciles_while_previous_preflight_is_blocked() {
        let gate = Arc::new(BuildGate::new());
        let mut router = RunnerRouter::new();
        router.register(Arc::new(FirstBuildBlockingRunner {
            gate: Arc::clone(&gate),
            builds: AtomicUsize::new(0),
        }));
        let api = api(router).await;
        let name = TaskId::from("latest-generation-wins");

        let first = api
            .write(
                routed(name.as_str(), 1_000),
                RuntimeSource::Routed,
                WriteMode::Create,
                true,
            )
            .await
            .unwrap();
        let first_done = first
            .reconciliation
            .expect("a created spec schedules reconciliation");
        wait_for_build(&gate).await;

        let second = tokio::time::timeout(
            Duration::from_millis(250),
            api.apply_task(routed(name.as_str(), 2_000)),
        )
        .await
        .expect("a newer desired commit must not wait for the old preflight")
        .unwrap();
        assert_eq!(second.metadata().generation(), 2);
        assert_eq!(second.status().phase, TaskPhase::Pending);

        let second_binding = wait_for_binding(&api, &name, 2).await;
        wait_for_observed(&api, &name, 2).await;
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), first_done)
            .await
            .expect("stale reconciliation did not finish")
            .expect("stale reconciliation acknowledgement dropped");
        assert_eq!(
            api.reconciler.state.binding_for(&name),
            Some(second_binding),
            "stale generation must not cancel or replace the current runtime"
        );
        api.delete_task(&name).await.unwrap();
        api.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_during_blocked_preflight_prevents_late_runtime_submission() {
        let gate = Arc::new(BuildGate::new());
        let runtime_started = Arc::new(AtomicBool::new(false));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(BlockingRunner {
            gate: Arc::clone(&gate),
            runtime_started: Arc::clone(&runtime_started),
        }));
        let api = api(router).await;
        let name = TaskId::from("delete-before-bind");

        let scheduled = api
            .write(
                routed(name.as_str(), 1_000),
                RuntimeSource::Routed,
                WriteMode::Create,
                true,
            )
            .await
            .unwrap();
        let reconciliation = scheduled
            .reconciliation
            .expect("a created spec schedules reconciliation");
        wait_for_build(&gate).await;

        tokio::time::timeout(Duration::from_millis(250), api.delete_task(&name))
            .await
            .expect("delete must not wait for runner preflight")
            .unwrap();
        assert!(api.get_task(&name).is_none());

        gate.release();
        tokio::time::timeout(Duration::from_secs(2), reconciliation)
            .await
            .expect("stale reconciliation did not finish")
            .expect("stale reconciliation acknowledgement dropped");
        assert!(api.get_task(&name).is_none());
        assert!(api.reconciler.state.binding_for(&name).is_none());
        assert!(
            !runtime_started.load(Ordering::Acquire),
            "a deleted resource must not be submitted after preflight"
        );

        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_started_rejects_desired_writes_without_committing_them() {
        let api = api(RunnerRouter::new()).await;
        api.shutdown().await.unwrap();

        let error = api
            .create_with_task(
                embedded("too-late", 1_000),
                immediate_task("too-late-runtime"),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::ShuttingDown));
        assert!(api.get_task(&TaskId::from("too-late")).is_none());
    }
}
