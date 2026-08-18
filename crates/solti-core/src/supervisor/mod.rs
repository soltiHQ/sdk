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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use solti_model::{
    ModelError, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRunPage,
    TaskRunQuery, TaskWorkload, WorkloadTypeMeta, WritePreconditions,
};
use solti_runner::RunnerRouter;
use taskvisor::{Supervisor, TaskRef};
#[cfg(test)]
use tokio::sync::oneshot;
use tracing::{debug, info, instrument};

use crate::{
    error::CoreError,
    output::{OutputHub, OutputSubscription},
    persistence::{
        TaskOutputSinkStatus, TaskStateSinkStatus, assert_persistence_sink_is_not_shutting_down,
    },
    runtime::{Reconciler, RuntimeObserver, RuntimeSource, TaskLocks},
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
    spawn_gate: parking_lot::Mutex<()>,
    shutdown_started: AtomicBool,
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
        let _gate = self.spawn_gate.lock();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.reconciler.state.close_watches();
        self.reconciler.retention_stop.cancel();
        self.reconciler.preflight_stop.cancel();
        self.reconciler.tasks.close();
        let handle = self.reconciler.handle.clone();
        let tasks = self.reconciler.tasks.clone();
        let state = self.reconciler.state.clone();
        let output_hub = Arc::clone(&self.reconciler.output_hub);
        let observer = Arc::clone(&self.reconciler.observer);
        let task = self.reconciler.runtime.spawn(async move {
            let shutdown_confirmed = handle.shutdown().await.is_ok();
            tasks.wait().await;
            if shutdown_confirmed {
                observer.finalize_pending_after_confirmed_shutdown().await;
            }
            output_hub.shutdown_persistence().await;
            state.shutdown_persistence().await;
        });
        drop(task);
    }
}

impl SupervisorApi {
    /// Creates a supervisor builder.
    pub fn builder(router: RunnerRouter) -> SupervisorApiBuilder {
        SupervisorApiBuilder::new(router)
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
            spawn_gate: parking_lot::Mutex::new(()),
            shutdown_started: AtomicBool::new(false),
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
                RuntimeSource::Routed,
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
        Self::ensure_runtime_contract(&manifest, &source)?;
        let name = manifest.name().clone();
        let operation = self.task_operations.lock(&name).await;
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::ShuttingDown);
        }
        let admission = self
            .reconciler
            .state
            .admit_state_write(StateMutationEventCapacity::TaskChange)
            .await
            .map_err(|_| CoreError::ShuttingDown)?;
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
        source: RuntimeSource,
        mode: WriteMode,
        preconditions: &WritePreconditions,
        ensure_output: bool,
        guards: WriteGuards,
    ) -> Result<ScheduledWrite, CoreError> {
        let WriteGuards {
            operation: _operation,
            admission,
        } = guards;
        Self::ensure_runtime_contract(&manifest, &source)?;
        let _spawn = self.spawn_gate.lock();
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::ShuttingDown);
        }

        let registration = self.reconciler.tasks.token();
        let commit = match mode {
            WriteMode::Create => {
                debug_assert!(preconditions.is_empty());
                self.reconciler
                    .state
                    .create_desired_admitted(&manifest, admission)?
            }
            WriteMode::Apply => self
                .reconciler
                .state
                .apply_desired_with_preconditions_admitted(&manifest, preconditions, admission)?,
        };
        if !commit.reconcile {
            drop(registration);
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
        // `TaskRef::drop` is user code and must not run under the global gate.
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
    /// Returns `None` when no output channel exists or the aggregate output
    /// budget cannot reserve one subscription's pending-event allowance.
    pub fn subscribe_output(&self, name: &TaskId) -> Option<OutputSubscription> {
        self.reconciler.output_hub.subscribe(name)
    }

    /// Subscribes through an adapter visibility predicate.
    ///
    /// The predicate, runtime binding, and subscription use the same per-name locks.
    /// The returned generation identifies the bound desired state.
    /// Returns `None` for missing, hidden, or unbound state, or when the
    /// aggregate output budget cannot admit the subscription.
    pub async fn subscribe_output_where<F>(
        &self,
        name: &TaskId,
        predicate: F,
    ) -> Option<(u64, OutputSubscription)>
    where
        F: Fn(&Task) -> bool,
    {
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

    /// Requests a terminal logical outcome for the current runtime while
    /// retaining desired state.
    ///
    /// A known task without a runtime binding is a no-op.
    /// Cancellation does not suppress later reconciliation.
    /// A Taskvisor `ForceAborted` outcome does not prove physical exit of
    /// non-cooperative task code.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for an unknown task.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(
        level = "debug",
        skip(self),
        fields(event = "task.cancel", task_name = %name)
    )]
    pub async fn cancel_task(&self, name: &TaskId) -> Result<(), CoreError> {
        let _operation = self.task_operations.lock(name).await;
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        let was_known = self.reconciler.state.contains_task(name);
        let cancellation = self.cancel_bound(name).await?;
        let claimed = cancellation.as_ref().is_some_and(|(_, claimed)| *claimed);
        if let Some((binding, _)) = cancellation {
            self.reconciler
                .observer
                .settle_after_confirmed_cleanup(binding.tv)
                .await;
        }
        if !claimed && !was_known {
            return Err(CoreError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Deletes a task and its run history.
    ///
    /// The current runtime reaches a terminal logical outcome first.
    /// A Taskvisor `ForceAborted` runtime may remain physically active after
    /// the resource and run history are removed.
    /// A missing task is an idempotent no-op.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ShuttingDown`] when state mutation admission has closed.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(
        level = "debug",
        skip(self),
        fields(event = "task.delete", task_name = %name)
    )]
    pub async fn delete_task(&self, name: &TaskId) -> Result<(), CoreError> {
        let _operation = self.task_operations.lock(name).await;
        self.delete_task_locked(name).await
    }

    /// Deletes a task after checking write preconditions.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when state mutation admission has closed.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    pub async fn delete_task_with_preconditions(
        &self,
        name: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), CoreError> {
        let _operation = self.task_operations.lock(name).await;
        let task = self
            .reconciler
            .state
            .get_retained(name)
            .ok_or_else(|| CoreError::NotFound(name.to_string()))?;
        TaskState::check_write_preconditions(&task, &preconditions)?;
        self.delete_task_locked(name).await
    }

    /// Deletes a task through an adapter visibility predicate.
    ///
    /// Missing and hidden tasks are reported as missing.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] when the task is missing or hidden.
    /// Returns [`CoreError::Conflict`] when a guard does not match.
    /// Returns [`CoreError::ShuttingDown`] when state mutation admission has closed.
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
        let _operation = self.task_operations.lock(name).await;
        let Some(task) = self.reconciler.state.get_retained(name) else {
            return Err(CoreError::NotFound(name.to_string()));
        };
        if !predicate(&task) {
            return Err(CoreError::NotFound(name.to_string()));
        }
        TaskState::check_write_preconditions(&task, &preconditions)?;
        self.delete_task_locked(name).await
    }

    async fn delete_task_locked(&self, name: &TaskId) -> Result<(), CoreError> {
        self.reconciler.cancel_scheduled(name);
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        debug!(
            event = "task.delete",
            task_name = %name,
            stage = "started",
            "deleting task"
        );
        let cancellation = self.cancel_bound(name).await?;
        let tv = cancellation.as_ref().map(|(binding, _)| binding.tv);
        self.reconciler
            .observer
            .delete_after_cleanup(name, tv)
            .await
            .map_err(|_| CoreError::ShuttingDown)?;
        Ok(())
    }

    /// Stops Taskvisor and waits for SDK-owned workers.
    ///
    /// Task watches close before runtime shutdown.
    /// Reconciliation, completion, retention, and persistence workers are drained.
    /// A Taskvisor `ForceAborted` outcome is logical: user task code that did
    /// not cooperate with cancellation may still exit physically afterward.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Supervisor`] when Taskvisor shutdown fails.
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
        {
            let _spawn = self.spawn_gate.lock();
            if !self.shutdown_started.swap(true, Ordering::AcqRel) {
                self.reconciler.state.close_watches();
                self.reconciler.retention_stop.cancel();
                self.reconciler.preflight_stop.cancel();
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
        if result.is_ok() {
            self.reconciler
                .observer
                .finalize_pending_after_confirmed_shutdown()
                .await;
        }
        self.reconciler.output_hub.shutdown_persistence().await;
        self.reconciler.state.shutdown_persistence().await;
        if result.is_ok() {
            info!(
                event = "supervisor.shutdown",
                stage = "completed",
                "supervisor shutdown completed"
            );
        }
        result
    }
}

#[cfg(test)]
mod tests;
