//! # Supervisor API
//!
//! [`SupervisorApi`] is the main lifecycle boundary of `solti-core`.
//! It commits a complete [`Task`] before runtime reconciliation.
//!
//! ## Write Flow
//!
//! ```text
//! TaskManifest
//!      │ validate and commit
//!      ▼
//! Task
//!      │ schedule
//!      ▼
//! Reconciler ──► runner or embedded TaskRef ──► Taskvisor
//! ```
//!
//! A successful write confirms the desired-state commit.
//! Reconciliation continues in an SDK-owned worker.
//!
//! ## Runtime Rules
//!
//! - A stale generation cannot bind or replace the current runtime.
//! - No staged rollout or availability guarantee is provided.
//! - Resource identity is `metadata.name` plus UID.
//! - Reconciliation is latest-wins by generation.
//! - Accepted side effects are not rolled back.

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use solti_model::{
    AgentCapabilities, ModelError, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery,
    TaskRunPage, TaskRunQuery, TaskWorkload, Uid, WorkloadTypeMeta, WritePreconditions,
};
use solti_runner::RunnerRouter;
use taskvisor::{Supervisor, TaskRef};
use tokio::sync::{oneshot, watch};
use tokio_util::task::TaskTracker;
use tracing::{debug, info, instrument, warn};

use crate::{
    error::CoreError,
    output::{OutputHub, OutputSubscription},
    persistence::{
        TaskOutputSinkStatus, TaskStateSinkStatus, assert_persistence_sink_is_not_shutting_down,
    },
    runtime::{
        GuardedRuntimeSource, Reconciler, RuntimeObserver, RuntimeSource, TaskLocks,
        guard_runtime_source,
    },
    state::{
        CollectionError, ResourceGeneration, RuntimeBinding, StateMutationEventCapacity,
        StateWriteAdmission, TaskState, TaskWatchSubscription,
    },
};

mod builder;
pub use builder::SupervisorApiBuilder;
use builder::SupervisorStartConfig;

/// Desired-state API over Taskvisor.
///
/// The API owns shared state, reconciliation, output, and retention.
/// Cloneable read access is available through [`state`](Self::state).
///
/// Dropping the API starts asynchronous cleanup.
/// Call [`shutdown`](Self::shutdown) to observe completion of the bounded
/// Taskvisor and SDK-owned cleanup workflow.
pub struct SupervisorApi {
    reconciler: Reconciler,
    task_operations: TaskLocks,
    delete_operations: TaskTracker,
    spawn_gate: parking_lot::Mutex<()>,
    shutdown_started: AtomicBool,
    shutdown: ShutdownCoordinator,
}

struct ShutdownCoordinator {
    operation: parking_lot::Mutex<Option<Arc<ShutdownOperation>>>,
}

struct ShutdownOperation {
    outcome: watch::Receiver<Option<ShutdownOutcome>>,
}

#[derive(Clone, Copy)]
enum ShutdownOutcome {
    Completed,
    SupervisorFailed,
    CoordinatorStopped,
}

impl ShutdownCoordinator {
    fn new() -> Self {
        Self {
            operation: parking_lot::Mutex::new(None),
        }
    }
}

impl ShutdownOperation {
    async fn wait(&self) -> ShutdownOutcome {
        let mut outcome = self.outcome.clone();
        loop {
            if let Some(outcome) = *outcome.borrow_and_update() {
                return outcome;
            }
            if outcome.changed().await.is_err() {
                return ShutdownOutcome::CoordinatorStopped;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    Create,
    Apply,
}

/// Desired-state commit and test-only reconciliation acknowledgement.
struct ScheduledWrite {
    committed: Task,
    #[cfg(test)]
    reconciliation: Option<oneshot::Receiver<Task>>,
}

struct WriteGuards {
    operation: tokio::sync::OwnedMutexGuard<()>,
    admission: StateWriteAdmission,
}

impl Drop for SupervisorApi {
    fn drop(&mut self) {
        drop(self.shutdown_operation());
    }
}

impl SupervisorApi {
    /// Creates a supervisor builder.
    pub fn builder(router: RunnerRouter) -> SupervisorApiBuilder {
        SupervisorApiBuilder::new(router)
    }

    /// Returns the capabilities of the immutable runner router used by this supervisor.
    pub fn runner_capabilities(&self) -> AgentCapabilities {
        self.reconciler.runner_capabilities()
    }

    fn begin_shutdown(&self) -> bool {
        let _spawn = self.spawn_gate.lock();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.reconciler.state.close_watches();
        self.reconciler.retention_stop.cancel();
        self.reconciler.preflight_stop.cancel();
        self.delete_operations.close();
        self.reconciler.tasks.close();
        true
    }

    fn shutdown_operation(&self) -> Arc<ShutdownOperation> {
        let mut installed = self.shutdown.operation.lock();
        if let Some(operation) = installed.as_ref() {
            return Arc::clone(operation);
        }

        self.begin_shutdown();
        let (outcome_tx, outcome_rx) = watch::channel(None);
        let operation = Arc::new(ShutdownOperation {
            outcome: outcome_rx,
        });
        *installed = Some(Arc::clone(&operation));
        let reconciler = self.reconciler.clone();
        let delete_operations = self.delete_operations.clone();
        let runtime = self.reconciler.runtime.clone();
        drop(installed);
        drop(runtime.spawn(async move {
            let outcome = Self::run_shutdown_coordinator(reconciler, delete_operations).await;
            outcome_tx.send_replace(Some(outcome));
        }));
        operation
    }

    async fn run_runtime_shutdown(reconciler: Reconciler) -> bool {
        let succeeded = reconciler.handle.clone().shutdown().await.is_ok();
        reconciler.tasks.wait().await;
        if succeeded {
            reconciler
                .observer
                .finalize_pending_after_confirmed_shutdown()
                .await;
        }
        succeeded
    }

    async fn run_shutdown_coordinator(
        reconciler: Reconciler,
        delete_operations: TaskTracker,
    ) -> ShutdownOutcome {
        delete_operations.wait().await;
        let runtime = reconciler.runtime.clone();
        let runtime_shutdown = runtime.spawn(Self::run_runtime_shutdown(reconciler.clone()));
        let runtime_outcome = match runtime_shutdown.await {
            Ok(true) => ShutdownOutcome::Completed,
            Ok(false) => ShutdownOutcome::SupervisorFailed,
            Err(error) => {
                Self::dispose_join_error(error);
                ShutdownOutcome::CoordinatorStopped
            }
        };

        let output_hub = Arc::clone(&reconciler.output_hub);
        let output_shutdown = runtime.spawn(async move {
            output_hub.shutdown_persistence().await;
        });
        let state = reconciler.state.clone();
        let state_shutdown = runtime.spawn(async move {
            state.shutdown_persistence().await;
        });
        let (output_result, state_result) = tokio::join!(output_shutdown, state_shutdown);
        let output_completed = Self::join_completed(output_result);
        let state_completed = Self::join_completed(state_result);
        if output_completed && state_completed {
            runtime_outcome
        } else {
            ShutdownOutcome::CoordinatorStopped
        }
    }

    fn join_completed(result: Result<(), tokio::task::JoinError>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) => {
                Self::dispose_join_error(error);
                false
            }
        }
    }

    fn dispose_join_error(error: tokio::task::JoinError) {
        if error.is_panic() {
            let payload = error.into_panic();
            if let Err(replacement) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(replacement);
            }
        }
    }

    async fn shutdown_result(&self, outcome: ShutdownOutcome) -> Result<(), CoreError> {
        match outcome {
            ShutdownOutcome::Completed => Ok(()),
            ShutdownOutcome::CoordinatorStopped => Err(CoreError::ShutdownCoordinatorStopped),
            ShutdownOutcome::SupervisorFailed => {
                match self.reconciler.handle.clone().shutdown().await {
                    Err(error) => Err(CoreError::supervisor("shutdown", error)),
                    Ok(()) => Err(CoreError::ShutdownCoordinatorStopped),
                }
            }
        }
    }

    async fn start(config: SupervisorStartConfig) -> Result<Self, CoreError> {
        let SupervisorStartConfig {
            runtime: sup_cfg,
            controller: ctrl_cfg,
            mut subscribers,
            router,
            state: state_cfg,
            reconciliation: reconciliation_cfg,
            output: output_config,
            persistence,
        } = config;
        let output_hub = Arc::new(
            OutputHub::try_with_sink(output_config, persistence.output, persistence.config)
                .map_err(CoreError::PersistenceInitialization)?,
        );
        let router = router.with_output_publisher(output_hub.clone());
        let state = TaskState::try_with_config_sink_and_persistence(
            state_cfg,
            persistence.state,
            persistence.config,
        )?;
        let observer = Arc::new(RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::clone(&output_hub),
        ));
        subscribers.push(observer.clone());

        let grace = sup_cfg.grace();
        let supervisor = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .try_build()
            .map_err(CoreError::SupervisorInitialization)?;
        let handle = supervisor
            .serve()
            .map_err(|error| CoreError::supervisor("start", error))?;
        let reconciler = Reconciler::new(
            output_hub,
            handle,
            router,
            state,
            observer,
            grace,
            reconciliation_cfg,
        );
        let api = Self {
            reconciler,
            task_operations: TaskLocks::default(),
            delete_operations: TaskTracker::new(),
            spawn_gate: parking_lot::Mutex::new(()),
            shutdown_started: AtomicBool::new(false),
            shutdown: ShutdownCoordinator::new(),
        };

        api.reconciler.spawn_retention_worker(state_cfg);
        info!(event = "supervisor.ready", "supervisor ready");
        Ok(api)
    }

    /// Creates a routed task resource.
    ///
    /// The resource is committed before reconciliation.
    /// Runtime failures are reported through the `Reconciled` condition.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidSpec`] for invalid desired state or an embedded workload.
    /// Returns [`CoreError::AlreadyExists`] when the name is retained.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when a new name cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when the
    /// manifest would exceed the retained byte budget.
    /// Returns [`CoreError::ShuttingDown`] after shutdown starts.
    pub async fn create_task(&self, manifest: TaskManifest) -> Result<Task, CoreError> {
        Ok(self
            .write(
                manifest,
                RuntimeSource::Routed,
                WriteMode::Create,
                WritePreconditions::new(),
                true,
            )
            .await?
            .committed)
    }

    /// Creates an embedded task resource.
    ///
    /// The caller supplies the Taskvisor task.
    /// Runtime failures are reported through the `Reconciled` condition.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidSpec`] for invalid desired state or a routed workload.
    /// Returns [`CoreError::AlreadyExists`] when the name is retained.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when a new name cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when the
    /// manifest would exceed the retained byte budget.
    /// Returns [`CoreError::ShuttingDown`] after shutdown starts.
    pub async fn create_embedded_task(
        &self,
        manifest: TaskManifest,
        task_ref: TaskRef,
    ) -> Result<Task, CoreError> {
        Ok(self
            .write(
                manifest,
                RuntimeSource::Prebuilt(task_ref),
                WriteMode::Create,
                WritePreconditions::new(),
                true,
            )
            .await?
            .committed)
    }

    /// Applies a routed task resource.
    ///
    /// A missing resource is created.
    /// An identical resource retries only when `Reconciled=False`.
    /// Runtime failures are reported through that condition.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidSpec`] for invalid desired state or an embedded workload.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when a missing name cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when a
    /// missing manifest or positive growth would exceed the retained byte budget.
    /// Returns [`CoreError::ShuttingDown`] after shutdown starts.
    pub async fn apply_task(&self, manifest: TaskManifest) -> Result<Task, CoreError> {
        self.apply_task_with_preconditions(manifest, WritePreconditions::new())
            .await
    }

    /// Applies a routed task after checking write preconditions.
    ///
    /// Non-empty preconditions prevent creation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when guarded state is missing.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when unguarded missing
    /// state cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when a
    /// missing manifest or positive growth would exceed the retained byte budget.
    /// Returns the same validation and shutdown errors as [`Self::apply_task`].
    pub async fn apply_task_with_preconditions(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, CoreError> {
        Ok(self
            .write(
                manifest,
                RuntimeSource::Routed,
                WriteMode::Apply,
                preconditions,
                true,
            )
            .await?
            .committed)
    }

    /// Applies a routed task through an adapter visibility predicate.
    ///
    /// A missing resource is created when preconditions are empty.
    /// A hidden resource is reported as missing.
    /// The predicate and commit share one per-name lock.
    /// The predicate must be pure and non-blocking. It must not call back into
    /// [`SupervisorApi`] because it runs synchronously while that lock is held.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for a hidden resource.
    /// Returns the same write errors as [`Self::apply_task_with_preconditions`].
    pub async fn apply_task_where<F>(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
        predicate: F,
    ) -> Result<Task, CoreError>
    where
        F: Fn(&Task) -> bool,
    {
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
        let admission = self
            .reconciler
            .state
            .admit_state_write(StateMutationEventCapacity::TaskChange)
            .await
            .map_err(|_| CoreError::ShuttingDown)?;
        Ok(self
            .write_locked(
                manifest,
                guard_runtime_source(RuntimeSource::Routed, name),
                WriteMode::Apply,
                &preconditions,
                true,
                WriteGuards {
                    operation,
                    admission,
                },
            )?
            .committed)
    }

    /// Applies an embedded task resource.
    ///
    /// A missing resource is created.
    /// An identical resource retries only when `Reconciled=False`.
    /// Runtime failures are reported through that condition.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvalidSpec`] for invalid desired state or a routed workload.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when a missing name cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when a
    /// missing manifest or positive growth would exceed the retained byte budget.
    /// Returns [`CoreError::ShuttingDown`] after shutdown starts.
    pub async fn apply_embedded_task(
        &self,
        manifest: TaskManifest,
        task_ref: TaskRef,
    ) -> Result<Task, CoreError> {
        self.apply_embedded_task_with_preconditions(manifest, task_ref, WritePreconditions::new())
            .await
    }

    /// Applies an embedded task after checking write preconditions.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when guarded state is missing.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when unguarded missing
    /// state cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when a
    /// missing manifest or positive growth would exceed the retained byte budget.
    /// Returns the same validation and shutdown errors as [`Self::apply_embedded_task`].
    pub async fn apply_embedded_task_with_preconditions(
        &self,
        manifest: TaskManifest,
        task_ref: TaskRef,
        preconditions: WritePreconditions,
    ) -> Result<Task, CoreError> {
        Ok(self
            .write(
                manifest,
                RuntimeSource::Prebuilt(task_ref),
                WriteMode::Apply,
                preconditions,
                true,
            )
            .await?
            .committed)
    }

    async fn write(
        &self,
        manifest: TaskManifest,
        source: RuntimeSource,
        mode: WriteMode,
        preconditions: WritePreconditions,
        ensure_output: bool,
    ) -> Result<ScheduledWrite, CoreError> {
        let name = manifest.name().clone();
        let source = guard_runtime_source(source, name.clone());
        Self::ensure_runtime_contract(&manifest, source.as_ref())?;
        let operation = self.task_operations.lock(&name).await;
        if self.shutdown_started.load(Ordering::Acquire) {
            drop(operation);
            source.dispose_at("shutdown_before_state_admission");
            return Err(CoreError::ShuttingDown);
        }
        let admission = match self
            .reconciler
            .state
            .admit_state_write(StateMutationEventCapacity::TaskChange)
            .await
        {
            Ok(admission) => admission,
            Err(_) => {
                drop(operation);
                source.dispose_at("state_admission_closed");
                return Err(CoreError::ShuttingDown);
            }
        };
        self.write_locked(
            manifest,
            source,
            mode,
            &preconditions,
            ensure_output,
            WriteGuards {
                operation,
                admission,
            },
        )
    }

    fn write_locked(
        &self,
        manifest: TaskManifest,
        source: GuardedRuntimeSource,
        mode: WriteMode,
        preconditions: &WritePreconditions,
        ensure_output: bool,
        guards: WriteGuards,
    ) -> Result<ScheduledWrite, CoreError> {
        let WriteGuards {
            operation,
            admission,
        } = guards;
        if let Err(error) = Self::ensure_runtime_contract(&manifest, source.as_ref()) {
            drop(admission);
            drop(operation);
            source.dispose_at("runtime_contract_rejected");
            return Err(error);
        }
        let _spawn = self.spawn_gate.lock();
        if self.shutdown_started.load(Ordering::Acquire) {
            drop(_spawn);
            drop(operation);
            source.dispose_at("shutdown_before_desired_commit");
            return Err(CoreError::ShuttingDown);
        }

        let registration = self.reconciler.tasks.token();
        let commit = match match mode {
            WriteMode::Create => {
                debug_assert!(preconditions.is_empty());
                self.reconciler
                    .state
                    .create_desired_admitted(&manifest, admission)
            }
            WriteMode::Apply => self
                .reconciler
                .state
                .apply_desired_with_preconditions_admitted(&manifest, preconditions, admission),
        } {
            Ok(commit) => commit,
            Err(error) => {
                drop(registration);
                drop(_spawn);
                drop(operation);
                source.dispose_at("desired_commit_rejected");
                return Err(error);
            }
        };
        if !commit.reconcile {
            drop(registration);
            drop(_spawn);
            drop(operation);
            source.dispose_at("desired_commit_did_not_schedule");
            return Ok(ScheduledWrite {
                committed: commit.task,
                #[cfg(test)]
                reconciliation: None,
            });
        }

        let committed = commit.task.clone();
        let (reconciliation, superseded) =
            self.reconciler
                .schedule(commit.task, source, ensure_output, registration);
        drop(_spawn);
        drop(operation);
        // `TaskRef::drop` is user code and must not run under coordination locks.
        drop(superseded);
        #[cfg(not(test))]
        drop(reconciliation);
        Ok(ScheduledWrite {
            committed,
            #[cfg(test)]
            reconciliation: Some(reconciliation),
        })
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
                "spec.workload kind Embedded requires create_embedded_task() or apply_embedded_task()"
                    .into(),
            ))),
        }
    }

    /// Subscribes to one task's live output.
    ///
    /// Returns `None` when no output channel exists, the channel is closed, or
    /// its subscriber count cannot be represented.
    pub fn subscribe_output(&self, name: &TaskId) -> Option<OutputSubscription> {
        self.reconciler.output_hub.subscribe(name)
    }

    /// Subscribes through an adapter visibility predicate.
    ///
    /// The UID check, predicate, runtime binding, and subscription use the same per-name locks.
    /// The predicate must be pure and non-blocking. It must not call back into
    /// [`SupervisorApi`] because it runs synchronously while those locks are held.
    /// The returned generation identifies the bound desired state.
    /// Returns `None` for a mismatched UID, missing, hidden, or unbound state,
    /// or when the output channel is closed or its subscriber count cannot be represented.
    pub async fn subscribe_output_where<F>(
        &self,
        name: &TaskId,
        task_uid: &Uid,
        predicate: F,
    ) -> Option<(u64, OutputSubscription)>
    where
        F: Fn(&Task) -> bool,
    {
        let _operation = self.task_operations.lock(name).await;
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        let task = self.reconciler.state.get_retained(name)?;
        if task.uid() != task_uid || !predicate(&task) {
            return None;
        }
        let resource = ResourceGeneration::from_task(&task);
        if self.reconciler.state.binding_for(name)?.resource != resource {
            return None;
        }
        let subscription = self
            .reconciler
            .output_hub
            .subscribe_if_uid(name, task_uid)?;
        Some((resource.generation, subscription))
    }

    /// Returns one retained task by name.
    pub fn get_task(&self, name: &TaskId) -> Option<Task> {
        self.reconciler.state.get(name)
    }

    /// Queries retained tasks with snapshot-consistent pagination.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_tasks(&self, query: &TaskQuery) -> Result<TaskPage<Task>, CollectionError> {
        self.reconciler.state.query(query)
    }

    /// Queries tasks through an adapter visibility predicate.
    ///
    /// The predicate runs before pagination.
    /// Core does not hide workload kinds by itself.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_tasks_where<F>(
        &self,
        query: &TaskQuery,
        predicate: F,
    ) -> Result<TaskPage<Task>, CollectionError>
    where
        F: Fn(&Task) -> bool,
    {
        self.reconciler.state.query_where(query, predicate)
    }

    /// Watches retained tasks selected by a filter.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] when the resource version cannot be resumed
    /// or Task watch admission is full.
    pub fn watch_tasks(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
    ) -> Result<TaskWatchSubscription, CollectionError> {
        self.reconciler.state.watch(filter, resource_version)
    }

    /// Watches tasks through an adapter visibility predicate.
    ///
    /// The predicate participates in `Added`, `Modified`, and `Deleted` classification.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] when the resource version cannot be resumed
    /// or Task watch admission is full.
    pub fn watch_tasks_where<F>(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
        predicate: F,
    ) -> Result<TaskWatchSubscription, CollectionError>
    where
        F: Fn(&Task) -> bool + Send + Sync + 'static,
    {
        self.reconciler
            .state
            .watch_where(filter, resource_version, predicate)
    }

    /// Queries a bounded TaskRun snapshot page.
    ///
    /// A first-page query returns `None` when the current task is absent.
    /// Continuations remain bound to the original Task UID.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_task_runs(
        &self,
        name: &TaskId,
        query: &TaskRunQuery,
    ) -> Result<Option<TaskRunPage>, CollectionError> {
        self.reconciler.state.query_runs(name, query)
    }

    /// Queries a bounded TaskRun snapshot through an adapter workload predicate.
    ///
    /// On a first page, the current task must exist and pass the predicate.
    /// Continuation pages reconstruct the original UID snapshot without
    /// consulting the current task. Historical runs are filtered by their
    /// workload snapshot before pagination.
    /// The first-page predicate must be pure and non-blocking. It must not call
    /// back into [`SupervisorApi`] because it runs synchronously while the
    /// per-name lock is held.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub async fn query_task_runs_where<F>(
        &self,
        name: &TaskId,
        query: &TaskRunQuery,
        predicate: F,
    ) -> Result<Option<TaskRunPage>, CollectionError>
    where
        F: Fn(&WorkloadTypeMeta) -> bool,
    {
        if query.continuation().is_none() {
            let _operation = self.task_operations.lock(name).await;
            return self.reconciler.state.query_runs_where_visible(
                name,
                query,
                |task| predicate(&task.spec().workload().type_meta()),
                |run| predicate(run.workload()),
            );
        }

        self.reconciler
            .state
            .query_runs_where(name, query, |run| predicate(run.workload()))
    }

    /// Returns a shared read handle.
    pub fn state(&self) -> TaskState {
        self.reconciler.state.clone()
    }

    /// Returns Taskvisor's point-in-time ownership and deferred-cleanup state.
    ///
    /// The finite capacity is shared by the core observer, external Taskvisor
    /// subscribers, accepted task values, and values awaiting isolated
    /// destruction. A destructor failure can permanently reduce the effective
    /// limit. The returned snapshot can become stale immediately.
    #[must_use = "inspect the returned Taskvisor ownership state"]
    pub fn ownership_snapshot(&self) -> crate::OwnershipSnapshot {
        self.reconciler.handle.ownership_snapshot()
    }

    /// Returns task-state persistence worker health and delivery counters.
    ///
    /// Returns `None` when no [`crate::TaskStateSink`] is installed.
    pub fn state_persistence_status(&self) -> Option<TaskStateSinkStatus> {
        self.reconciler.state.persistence_status()
    }

    /// Returns task-output persistence worker health and delivery counters.
    ///
    /// Returns `None` when no [`crate::TaskOutputSink`] is installed.
    pub fn output_persistence_status(&self) -> Option<TaskOutputSinkStatus> {
        self.reconciler.output_hub.persistence_status()
    }

    async fn cancel_bound(
        reconciler: &Reconciler,
        name: &TaskId,
    ) -> Result<Option<(RuntimeBinding, bool)>, CoreError> {
        let Some(binding) = reconciler.state.binding_for(name) else {
            return Ok(None);
        };
        let claimed = reconciler
            .handle
            .cancel(binding.tv)
            .termination_timeout(reconciler.grace.saturating_add(Duration::from_secs(1)))
            .execute()
            .await
            .map_err(|error| CoreError::supervisor("cancel", error))?;
        Ok(Some((binding, claimed)))
    }

    async fn run_cancel_task(
        reconciler: Reconciler,
        task_operations: TaskLocks,
        name: TaskId,
    ) -> Result<(), CoreError> {
        let operation = task_operations.lock(&name).await;
        if !reconciler.state.contains_task(&name) {
            return Err(CoreError::NotFound(name.to_string()));
        }
        Self::run_cancel_task_owned(reconciler, name, operation).await
    }

    async fn run_cancel_task_owned(
        reconciler: Reconciler,
        name: TaskId,
        _operation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), CoreError> {
        if let Some(settled) = reconciler.cancel_scheduled_for_user(&name) {
            settled.cancelled().await;
        }
        let _runtime_operation = reconciler.runtime_operations.lock(&name).await;
        let cancellation = Self::cancel_bound(&reconciler, &name).await?;
        if let Some((binding, _)) = cancellation {
            reconciler
                .observer
                .settle_after_confirmed_cleanup(binding.tv)
                .await;
        }
        Ok(())
    }

    async fn await_cancel_task<F>(&self, name: &TaskId, operation: F) -> Result<(), CoreError>
    where
        F: Future<Output = Result<(), CoreError>> + Send + 'static,
    {
        let completion = {
            let _spawn = self.spawn_gate.lock();
            if self.shutdown_started.load(Ordering::Acquire) {
                return Err(CoreError::ShuttingDown);
            }

            let (sender, completion) = oneshot::channel();
            let runtime = self.reconciler.runtime.clone();
            let worker = self.reconciler.tasks.spawn_on(
                async move {
                    let result = operation.await;
                    let _ = sender.send(result);
                },
                &runtime,
            );
            drop(worker);
            completion
        };

        match completion.await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    event = "task.cancel_unavailable",
                    task_name = %name,
                    "supervisor-owned cancellation worker became unavailable"
                );
                Err(CoreError::ShuttingDown)
            }
        }
    }

    /// Stops current reconciliation or requests a terminal logical outcome for
    /// the current runtime while retaining desired state.
    ///
    /// Scheduled reconciliation is cancelled before waiting for the per-task
    /// runtime lock. If Taskvisor has not accepted the submission, core drops
    /// the prepared submission and records `Reconciled=False` without creating
    /// a TaskRun. If shutdown's preflight stop completes that branch first, the
    /// existing intake-pending `Reconciled=Unknown` condition remains instead.
    /// Accepted queued or running work is cancelled by Taskvisor ID. After the
    /// supervisor worker is registered, dropping this method's future stops
    /// only the caller's wait. Shutdown drains the registered operation.
    /// Cancellation does not suppress later reconciliation.
    /// A Taskvisor `ForceAborted` outcome does not prove physical exit of
    /// non-cooperative task code.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for an unknown task.
    /// Returns [`CoreError::ShuttingDown`] when shutdown has closed operation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(
        level = "debug",
        skip(self),
        fields(event = "task.cancel", task_name = %name)
    )]
    pub async fn cancel_task(&self, name: &TaskId) -> Result<(), CoreError> {
        let reconciler = self.reconciler.clone();
        let task_operations = self.task_operations.clone();
        let worker_name = name.clone();
        self.await_cancel_task(name, async move {
            Self::run_cancel_task(reconciler, task_operations, worker_name).await
        })
        .await
    }

    /// Stops current reconciliation or runtime after checking write preconditions.
    ///
    /// The resource check and cancellation are serialized with create, apply,
    /// delete, and other cancellation operations for the same task name.
    /// Desired state and run history remain retained.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when shutdown has closed operation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    pub async fn cancel_task_with_preconditions(
        &self,
        name: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), CoreError> {
        self.cancel_task_where(name, preconditions, |_| true).await
    }

    /// Stops current reconciliation or runtime through an adapter visibility predicate.
    ///
    /// Missing and hidden tasks are reported as missing. The visibility and
    /// precondition checks are atomic with respect to other operations for the
    /// same task name. The predicate must be pure and non-blocking. It must not
    /// call back into [`SupervisorApi`] because it runs synchronously while the
    /// per-name lock is held. Desired state and run history remain retained.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing or hidden.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when shutdown has closed operation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    pub async fn cancel_task_where<F>(
        &self,
        name: &TaskId,
        preconditions: WritePreconditions,
        predicate: F,
    ) -> Result<(), CoreError>
    where
        F: Fn(&Task) -> bool,
    {
        let operation = self.task_operations.lock(name).await;
        let Some(task) = self.reconciler.state.get_retained(name) else {
            return Err(CoreError::NotFound(name.to_string()));
        };
        if !predicate(&task) {
            return Err(CoreError::NotFound(name.to_string()));
        }
        TaskState::check_write_preconditions(&task, &preconditions)?;

        let reconciler = self.reconciler.clone();
        let worker_name = name.clone();
        self.await_cancel_task(name, async move {
            Self::run_cancel_task_owned(reconciler, worker_name, operation).await
        })
        .await
    }

    /// Deletes a task and its run history.
    ///
    /// The current runtime reaches a terminal logical outcome first.
    /// A Taskvisor `ForceAborted` runtime may remain physically active after
    /// the resource and run history are removed.
    /// A missing task is an idempotent no-op that returns before SDK-owned
    /// delete registration or persistence admission. After the delete worker
    /// is registered, dropping this method's future stops only the caller's
    /// wait. Shutdown drains the registered operation.
    ///
    /// # Errors
    ///
    /// For a retained task, returns [`CoreError::ShuttingDown`] when shutdown
    /// has closed operation or state mutation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(
        level = "debug",
        skip(self),
        fields(event = "task.delete", task_name = %name)
    )]
    pub async fn delete_task(&self, name: &TaskId) -> Result<(), CoreError> {
        let operation = self.task_operations.lock(name).await;
        if !self.reconciler.state.contains_task(name) {
            return Ok(());
        }
        self.delete_task_owned(name, operation).await
    }

    /// Deletes a task after checking write preconditions.
    ///
    /// After the delete worker is registered, dropping this method's future
    /// stops only the caller's wait. Shutdown drains the registered operation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when shutdown has closed operation
    /// or state mutation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    pub async fn delete_task_with_preconditions(
        &self,
        name: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), CoreError> {
        let operation = self.task_operations.lock(name).await;
        let task = self
            .reconciler
            .state
            .get_retained(name)
            .ok_or_else(|| CoreError::NotFound(name.to_string()))?;
        TaskState::check_write_preconditions(&task, &preconditions)?;
        self.delete_task_owned(name, operation).await
    }

    /// Deletes a task through an adapter visibility predicate.
    ///
    /// Missing and hidden tasks are reported as missing.
    /// The predicate must be pure and non-blocking. It must not call back into
    /// [`SupervisorApi`] because it runs synchronously while the per-name lock is held.
    /// After the delete worker is registered, dropping this method's future
    /// stops only the caller's wait. Shutdown drains the registered operation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing or hidden.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when shutdown has closed operation
    /// or state mutation admission.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    pub async fn delete_task_where<F>(
        &self,
        name: &TaskId,
        preconditions: WritePreconditions,
        predicate: F,
    ) -> Result<(), CoreError>
    where
        F: Fn(&Task) -> bool,
    {
        let operation = self.task_operations.lock(name).await;
        let Some(task) = self.reconciler.state.get_retained(name) else {
            return Err(CoreError::NotFound(name.to_string()));
        };
        if !predicate(&task) {
            return Err(CoreError::NotFound(name.to_string()));
        }
        TaskState::check_write_preconditions(&task, &preconditions)?;
        self.delete_task_owned(name, operation).await
    }

    async fn run_delete_task(
        reconciler: Reconciler,
        name: TaskId,
        _operation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), CoreError> {
        if let Some(settled) = reconciler.cancel_scheduled(&name) {
            settled.cancelled().await;
        }
        let _runtime_operation = reconciler.runtime_operations.lock(&name).await;
        debug!(
            event = "task.delete",
            task_name = %name,
            stage = "started",
            "deleting task"
        );
        let cancellation = Self::cancel_bound(&reconciler, &name).await?;
        let tv = cancellation.as_ref().map(|(binding, _)| binding.tv);
        reconciler
            .observer
            .delete_after_cleanup(&name, tv)
            .await
            .map_err(|_| CoreError::ShuttingDown)?;
        Ok(())
    }

    /// Registers one checked delete and transfers its per-name guard to an
    /// SDK-owned worker.
    async fn delete_task_owned(
        &self,
        name: &TaskId,
        operation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), CoreError> {
        let completion = {
            let _spawn = self.spawn_gate.lock();
            if self.shutdown_started.load(Ordering::Acquire) {
                return Err(CoreError::ShuttingDown);
            }

            let (sender, completion) = oneshot::channel();
            let reconciler = self.reconciler.clone();
            let worker_name = name.clone();
            let runtime = self.reconciler.runtime.clone();
            let worker = self.delete_operations.spawn_on(
                async move {
                    let result = Self::run_delete_task(reconciler, worker_name, operation).await;
                    let _ = sender.send(result);
                },
                &runtime,
            );
            drop(worker);
            completion
        };

        match completion.await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    event = "task.delete_unavailable",
                    task_name = %name,
                    "supervisor-owned delete worker became unavailable"
                );
                Err(CoreError::ShuttingDown)
            }
        }
    }

    /// Stops Taskvisor and waits for SDK-owned workers.
    ///
    /// Accepted delete operations drain before Taskvisor shutdown starts.
    /// Task watches close before runtime shutdown.
    /// Reconciliation, completion, retention, and persistence workers are drained.
    /// A Taskvisor `ForceAborted` outcome is logical: user task code that did
    /// not cooperate with cancellation may still exit physically afterward.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Supervisor`] when Taskvisor shutdown fails.
    /// Returns [`CoreError::ShutdownCoordinatorStopped`] when an SDK-owned
    /// shutdown or persistence worker stops unexpectedly.
    ///
    /// # Panics
    ///
    /// Panics when this future is polled on a [`crate::TaskStateSink`] or
    /// [`crate::TaskOutputSink`] callback worker. Shutdown would otherwise wait
    /// for its own callback.
    #[instrument(level = "info", skip(self), fields(event = "supervisor.shutdown"))]
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        assert_persistence_sink_is_not_shutting_down();
        info!(
            event = "supervisor.shutdown",
            stage = "started",
            "supervisor shutdown started"
        );
        let operation = self.shutdown_operation();
        let result = self.shutdown_result(operation.wait().await).await;
        if result.is_ok() {
            info!(
                event = "supervisor.shutdown",
                stage = "completed",
                "supervisor shutdown completed"
            );
        }
        result
    }

    /// Stops Taskvisor and waits at most `timeout` for SDK-owned cleanup.
    ///
    /// This method has the same ordering and lossless persistence semantics as
    /// [`Self::shutdown`]. If the deadline elapses, the accepted shutdown
    /// coordinator remains detached and continues draining in the background.
    /// No callback or task is forcefully terminated by this deadline.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ShutdownTimedOut`] when `timeout` elapses first.
    /// Other shutdown failures are the same as [`Self::shutdown`]. Every
    /// caller joins the same cached SDK-owned operation.
    ///
    /// # Panics
    ///
    /// Panics under the same callback-worker condition as [`Self::shutdown`].
    #[instrument(
        level = "info",
        skip(self),
        fields(event = "supervisor.shutdown", timeout_ms = timeout.as_millis())
    )]
    pub async fn shutdown_with_timeout(&self, timeout: Duration) -> Result<(), CoreError> {
        assert_persistence_sink_is_not_shutting_down();
        info!(
            event = "supervisor.shutdown",
            stage = "started",
            timeout_ms = timeout.as_millis(),
            "supervisor shutdown with a caller deadline started"
        );
        let operation = self.shutdown_operation();
        match tokio::time::timeout(timeout, async {
            self.shutdown_result(operation.wait().await).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(CoreError::ShutdownTimedOut { timeout }),
        }
    }
}

#[cfg(test)]
mod tests;
