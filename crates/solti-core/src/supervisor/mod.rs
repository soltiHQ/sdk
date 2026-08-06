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
    ModelError, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRun, TaskWorkload,
    WorkloadTypeMeta, WritePreconditions,
};
use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, Subscribe, Supervisor, SupervisorConfig, TaskRef};
use tokio::sync::oneshot;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tracing::{debug, info, instrument};

use crate::{
    StateConfig,
    error::CoreError,
    output::{OutputConfig, OutputHub, OutputSubscription},
    persistence::PersistenceSinks,
    runtime::{Reconciler, RuntimeObserver, RuntimeSource, TaskLocks},
    state::{
        CollectionError, DesiredCommit, ResourceGeneration, RuntimeBinding, TaskState,
        TaskWatchSubscription,
    },
};

mod builder;
pub use builder::SupervisorApiBuilder;

/// Desired-state API over Taskvisor.
///
/// The API owns shared state, reconciliation, output, and retention.
/// Cloneable read access is available through [`state`](Self::state).
///
/// Dropping the API starts asynchronous cleanup.
/// Call [`shutdown`](Self::shutdown) when cleanup must finish before continuing.
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

impl Drop for SupervisorApi {
    fn drop(&mut self) {
        let _gate = self.spawn_gate.lock();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.reconciler.state.close_watches();
        self.reconciler.retention_stop.cancel();
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
    /// Creates a supervisor builder.
    pub fn builder(router: RunnerRouter) -> SupervisorApiBuilder {
        SupervisorApiBuilder::new(router)
    }

    async fn start(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
        output_config: OutputConfig,
        persistence: PersistenceSinks,
    ) -> Result<Self, CoreError> {
        let mut subscribers = subscribers;
        let output_hub = Arc::new(OutputHub::with_sink(output_config, persistence.output));
        let router = router.with_output_publisher(output_hub.clone());
        let state = TaskState::try_with_config_and_sink(state_cfg, persistence.state)?;
        let observer = Arc::new(RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::clone(&output_hub),
        ));
        subscribers.push(observer.clone());

        let grace = sup_cfg.grace();
        let supervisor = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();
        let handle = supervisor.serve();
        let reconciler = Reconciler::new(output_hub, handle, router, state, observer, grace);
        let api = Self {
            reconciler,
            task_operations: TaskLocks::default(),
            spawn_gate: parking_lot::Mutex::new(()),
            shutdown_started: AtomicBool::new(false),
        };

        api.reconciler.spawn_retention_worker(state_cfg);
        info!("supervisor is ready");
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
        Ok(self
            .write_locked(
                manifest,
                RuntimeSource::Routed,
                WriteMode::Apply,
                &preconditions,
                true,
                operation,
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
        self.write_locked(
            manifest,
            source,
            mode,
            &preconditions,
            ensure_output,
            operation,
        )
    }

    fn write_locked(
        &self,
        manifest: TaskManifest,
        source: RuntimeSource,
        mode: WriteMode,
        preconditions: &WritePreconditions,
        ensure_output: bool,
        _operation: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ScheduledWrite, CoreError> {
        Self::ensure_runtime_contract(&manifest, &source)?;
        let _spawn = self.spawn_gate.lock();
        if self.shutdown_started.load(Ordering::Acquire) {
            return Err(CoreError::ShuttingDown);
        }

        let registration = self.reconciler.tasks.token();
        let commit = match mode {
            WriteMode::Create => {
                debug_assert!(preconditions.is_empty());
                self.reconciler.state.create_desired(&manifest)?
            }
            WriteMode::Apply => self
                .reconciler
                .state
                .apply_desired_with_preconditions(&manifest, preconditions)?,
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
        let reconciliation = self.spawn_reconciliation(commit, source, ensure_output, registration);
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

    /// Subscribes to one task's live output.
    ///
    /// Returns `None` when no output channel exists.
    pub fn subscribe_output(&self, name: &TaskId) -> Option<OutputSubscription> {
        self.reconciler.output_hub.subscribe(name)
    }

    /// Subscribes through an adapter visibility predicate.
    ///
    /// The predicate, runtime binding, and subscription use the same per-name locks.
    /// The returned generation identifies the bound desired state.
    /// Returns `None` for missing, hidden, or unbound state.
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
    /// Returns [`CollectionError`] when the resource version cannot be resumed.
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
    /// Returns [`CollectionError`] when the resource version cannot be resumed.
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

    /// Lists retained runs for one task.
    ///
    /// Results are ordered by generation and attempt.
    pub fn list_task_runs(&self, name: &TaskId) -> Vec<TaskRun> {
        self.reconciler.state.list_runs(name)
    }

    /// Lists runs through an adapter workload predicate.
    ///
    /// The visibility check and run snapshot share the write-operation lock.
    /// Historical runs are filtered by their workload snapshot.
    /// Returns `None` when the current task is missing or hidden.
    pub async fn list_task_runs_where<F>(&self, name: &TaskId, predicate: F) -> Option<Vec<TaskRun>>
    where
        F: Fn(&WorkloadTypeMeta) -> bool,
    {
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
                .filter(|run| predicate(run.workload()))
                .collect(),
        )
    }

    /// Returns a shared read handle.
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

    /// Cancels the current runtime while retaining desired state.
    ///
    /// A known task without a runtime binding is a no-op.
    /// Cancellation does not suppress later reconciliation.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for an unknown task.
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(level = "debug", skip(self), fields(task = %name))]
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
    /// The current runtime is stopped first.
    /// A missing task is an idempotent no-op.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Supervisor`] when Taskvisor cancellation fails.
    #[instrument(level = "debug", skip(self), fields(task = %name))]
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
        let _runtime_operation = self.reconciler.runtime_operations.lock(name).await;
        debug!(task = %name, "deleting task resource");
        let cancellation = self.cancel_bound(name).await?;
        let tv = cancellation.as_ref().map(|(binding, _)| binding.tv);
        self.reconciler.observer.delete_after_cleanup(name, tv);
        Ok(())
    }

    /// Stops Taskvisor and waits for SDK-owned workers.
    ///
    /// Task watches close before runtime shutdown.
    /// Reconciliation, completion, and retention workers are drained.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Supervisor`] when Taskvisor shutdown fails.
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        info!("initiating graceful shutdown");
        {
            let _spawn = self.spawn_gate.lock();
            if !self.shutdown_started.swap(true, Ordering::AcqRel) {
                self.reconciler.state.close_watches();
                self.reconciler.retention_stop.cancel();
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
        AdmissionPolicy, ConditionStatus, EmbeddedSpec, Flag, Labels, Slot, SubprocessMode,
        SubprocessSpec, TaskEnv, TaskPhase, TaskSpec, TaskWorkload, WORKLOAD_API_VERSION,
        WorkloadTypeMeta,
    };
    use solti_runner::{BuildContext, RunId, Runner, RunnerError};
    use taskvisor::{TaskContext, TaskError, TaskFn};
    use tokio_stream::StreamExt;

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

    fn subprocess_workload_types() -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess")
                .expect("built-in workload GVK"),
        ]
    }

    fn retention_slot(name: &str) -> TaskManifest {
        TaskManifest::new(
            name,
            TaskSpec::builder(
                "solti-state-sweep",
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
        SupervisorApi::builder(router).start().await.unwrap()
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
            task.status().observed_generation() == generation
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
            condition.observed_generation() == generation && condition.status() == status
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
        fn name(&self) -> &str {
            "recording"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            subprocess_workload_types()
        }

        fn build_task(
            &self,
            task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            self.seen.lock().push((
                task.name().clone(),
                task.metadata().generation(),
                task.metadata().resource_version().to_string(),
            ));
            Ok(immediate_task(run_id.name()))
        }
    }

    #[tokio::test]
    async fn stale_generation_is_rejected_before_runner_build() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(RecordingRunner {
                seen: Arc::clone(&seen),
            }))
            .unwrap();
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
                .binding_for(&TaskId::new("stale-before-build").unwrap())
                .is_none()
        );
        api.reconciler
            .state
            .delete_task(&TaskId::new("stale-before-build").unwrap());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn all_four_resource_write_paths_accept_desired_manifests() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(RecordingRunner {
                seen: Arc::clone(&seen),
            }))
            .unwrap();
        let api = api(router).await;

        let created = api
            .create_task(routed("routed-resource", 1_000))
            .await
            .unwrap();
        assert_eq!(created.name().as_str(), "routed-resource");
        assert!(!created.metadata().resource_version().is_empty());
        assert_eq!(created.status().phase(), TaskPhase::Pending);
        assert_eq!(created.status().observed_generation(), 0);
        wait_for_observed(&api, created.name(), 1).await;

        let mut labels = Labels::new();
        labels.insert("team", "platform");
        let metadata_apply = TaskManifest::new("routed-resource", created.spec().clone())
            .unwrap()
            .with_labels(labels.clone())
            .unwrap();
        let applied = api.apply_task(metadata_apply).await.unwrap();
        assert_eq!(applied.metadata().generation(), 1);
        assert_eq!(applied.metadata().labels(), &labels);

        let applied = api
            .apply_task(routed("routed-resource", 2_000))
            .await
            .unwrap();
        assert_eq!(applied.metadata().generation(), 2);
        assert_eq!(applied.status().phase(), TaskPhase::Pending);
        assert_eq!(applied.status().observed_generation(), 1);
        wait_for_observed(&api, applied.name(), 2).await;

        let embedded_created = api
            .create_embedded_task(
                embedded("embedded-resource", 1_000),
                immediate_task("unrelated-runtime-name"),
            )
            .await
            .unwrap();
        assert_eq!(embedded_created.name().as_str(), "embedded-resource");
        assert_eq!(embedded_created.status().phase(), TaskPhase::Pending);
        wait_for_observed(&api, embedded_created.name(), 1).await;
        assert!(
            api.get_task(&TaskId::new("unrelated-runtime-name").unwrap())
                .is_none()
        );

        let embedded_applied = api
            .apply_embedded_task(
                embedded("embedded-resource", 2_000),
                immediate_task("another-runtime-name"),
            )
            .await
            .unwrap();
        assert_eq!(embedded_applied.metadata().generation(), 2);
        assert_eq!(embedded_applied.status().phase(), TaskPhase::Pending);
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
            .create_embedded_task(
                embedded_with_revision("embedded-revision", 10_000, "v1"),
                cancellable_task("runtime-v1"),
            )
            .await
            .unwrap();
        let first_binding = wait_for_binding(&api, first.name(), 1).await;

        let unchanged = api
            .apply_embedded_task(
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
            .apply_embedded_task(
                embedded_with_revision("embedded-revision", 10_000, "v2"),
                cancellable_task("runtime-v2"),
            )
            .await
            .unwrap();
        assert_eq!(changed.metadata().generation(), 2);
        assert_eq!(changed.status().phase(), TaskPhase::Pending);
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
            .create_embedded_task(
                routed("prebuilt-routed", 1_000),
                immediate_task("arbitrary-runtime"),
            )
            .await;
        assert!(matches!(prebuilt_routed, Err(CoreError::InvalidSpec(_))));
        assert!(
            api.get_task(&TaskId::new("prebuilt-routed").unwrap())
                .is_none()
        );

        let routed_embedded = api.create_task(embedded("routed-embedded", 1_000)).await;
        assert!(matches!(routed_embedded, Err(CoreError::InvalidSpec(_))));
        assert!(
            api.get_task(&TaskId::new("routed-embedded").unwrap())
                .is_none()
        );

        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retention_worker_does_not_reserve_a_resource_name_or_slot() {
        let api = api(RunnerRouter::new()).await;
        let sweep_name = TaskId::new("solti-state-sweep").unwrap();
        assert!(api.get_task(&sweep_name).is_none());

        api.create_embedded_task(
            embedded(sweep_name.as_str(), 1_000),
            immediate_task("former-sweep-name"),
        )
        .await
        .unwrap();
        api.create_embedded_task(
            retention_slot("former-sweep-slot"),
            immediate_task("former-sweep-slot-runtime"),
        )
        .await
        .unwrap();

        assert!(api.get_task(&sweep_name).is_some());
        assert_eq!(
            api.query_tasks(&TaskQuery::new().with_slot(Slot::new("solti-state-sweep").unwrap()))
                .unwrap()
                .items
                .len(),
            1
        );
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retention_worker_removes_expired_terminal_resources() {
        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO)
            .try_with_sweep_interval(Duration::from_millis(1))
            .unwrap();
        let api = SupervisorApi::builder(RunnerRouter::new())
            .with_state_config(config)
            .start()
            .await
            .unwrap();
        let task = api
            .create_embedded_task(
                embedded("retained-briefly", 1_000),
                immediate_task("retained-briefly-runtime"),
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            while api.get_task(task.name()).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retention worker did not remove the terminal resource");

        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn conditional_reads_and_delete_share_the_resource_operation_lock() {
        let api = api(RunnerRouter::new()).await;
        let task = api
            .create_embedded_task(
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
            api.delete_task_where(task.name(), WritePreconditions::new(), |_| false)
                .await,
            Err(CoreError::NotFound(_))
        ));
        assert!(api.get_task(task.name()).is_some());
        assert!(matches!(
            api.delete_task_where(
                &TaskId::new("missing").unwrap(),
                WritePreconditions::new(),
                |_| true,
            )
            .await,
            Err(CoreError::NotFound(_))
        ));
        api.delete_task_where(task.name(), WritePreconditions::new(), |_| true)
            .await
            .unwrap();
        assert!(api.get_task(task.name()).is_none());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn checked_delete_rejects_stale_uid_before_removing_the_resource() {
        let api = api(RunnerRouter::new()).await;
        let task = api
            .create_task(routed("checked-delete", 1_000))
            .await
            .unwrap();
        let stale = WritePreconditions::new().with_uid(solti_model::Uid::new("stale-uid").unwrap());

        let error = api
            .delete_task_with_preconditions(task.name(), stale)
            .await
            .unwrap_err();

        assert!(matches!(error, CoreError::Conflict(_)));
        assert!(api.get_task(task.name()).is_some());

        let current = api.get_task(task.name()).unwrap();
        let matching = WritePreconditions::new().with_uid(current.uid().clone());
        api.delete_task_with_preconditions(task.name(), matching)
            .await
            .unwrap();
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
        assert_eq!(visible[0].generation(), 2);
        assert_eq!(visible[0].workload().kind(), "Subprocess");

        state.delete_task(current.name());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn conditional_apply_cannot_replace_a_hidden_existing_resource() {
        let api = api(RunnerRouter::new()).await;
        let embedded = api
            .create_embedded_task(
                embedded("hidden-apply", 10_000),
                cancellable_task("hidden-runtime"),
            )
            .await
            .unwrap();

        let result = api
            .apply_task_where(
                routed("hidden-apply", 1_000),
                WritePreconditions::new(),
                |current| !matches!(current.spec().workload(), TaskWorkload::Embedded(_)),
            )
            .await;

        assert!(matches!(result, Err(CoreError::NotFound(_))));
        assert_eq!(api.get_task(embedded.name()), Some(embedded.clone()));

        let created = api
            .apply_task_where(
                routed("new-visible", 1_000),
                WritePreconditions::new(),
                |_| panic!("predicate must not run for an absent resource"),
            )
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

        assert_eq!(task.status().phase(), TaskPhase::Pending);
        assert_eq!(task.status().observed_generation(), 0);
        let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().phase(), TaskPhase::Pending);
        assert_eq!(failed.status().attempt(), 0);
        assert!(failed.status().error().is_none());
        assert_eq!(failed.status().reconciled().reason(), "RunnerNotFound");
        assert!(
            failed
                .status()
                .reconciled()
                .message()
                .contains("no runner matches")
        );
        assert_eq!(api.get_task(task.name()), Some(failed));
        api.shutdown().await.unwrap();
    }

    struct PanicRunner;

    impl Runner for PanicRunner {
        fn name(&self) -> &str {
            "panic"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            subprocess_workload_types()
        }

        fn build_task(
            &self,
            _task: &Task,
            _run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            panic!("runner build panic")
        }
    }

    #[tokio::test]
    async fn runner_panic_is_contained_as_reconciliation_failure() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(PanicRunner)).unwrap();
        let api = api(router).await;

        let task = api
            .create_task(routed("panic-contained", 1_000))
            .await
            .expect("desired state remains queryable");

        assert_eq!(task.status().phase(), TaskPhase::Pending);
        assert_eq!(task.status().observed_generation(), 0);
        let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().phase(), TaskPhase::Pending);
        assert_eq!(failed.status().attempt(), 0);
        assert!(failed.status().error().is_none());
        assert_eq!(failed.status().reconciled().reason(), "RunnerBuildPanicked");
        assert_eq!(
            failed.status().reconciled().message(),
            "reconciliation preflight panicked"
        );
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_new_generation_does_not_cancel_the_old_runtime() {
        let api = api(RunnerRouter::new()).await;
        let first = api
            .create_embedded_task(embedded("upgrade", 10_000), cancellable_task("old-runtime"))
            .await
            .unwrap();
        let previous = wait_for_binding(&api, first.name(), 1).await;

        let failed = api.apply_task(routed("upgrade", 2_000)).await.unwrap();
        assert_eq!(failed.metadata().generation(), 2);
        assert_eq!(failed.status().phase(), TaskPhase::Pending);
        assert_eq!(failed.status().observed_generation(), 1);
        let failed = wait_for_reconciled(&api, failed.name(), 2, ConditionStatus::False).await;
        assert_eq!(failed.status().phase(), TaskPhase::Pending);
        assert_eq!(failed.status().reconciled().reason(), "RunnerNotFound");
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
        fn name(&self) -> &str {
            "fail-once-blocking"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            subprocess_workload_types()
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
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
            Ok(immediate_task(run_id.name()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_apply_retries_once_only_while_reconciled_is_false() {
        let builds = Arc::new(AtomicUsize::new(0));
        let retry_gate = Arc::new(BuildGate::new());
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(FailOnceBlockingRunner {
                builds: Arc::clone(&builds),
                retry_gate: Arc::clone(&retry_gate),
            }))
            .unwrap();
        let api = api(router).await;
        let manifest = routed("manual-retry", 1_000);

        let created = api.create_task(manifest.clone()).await.unwrap();
        let failed = wait_for_reconciled(&api, created.name(), 1, ConditionStatus::False).await;
        assert_eq!(failed.status().reconciled().reason(), "RunnerBuildFailed");

        let retry = api.apply_task(manifest.clone()).await.unwrap();
        assert_eq!(retry.metadata().generation(), 1);
        assert_eq!(
            retry.status().reconciled().status(),
            ConditionStatus::Unknown
        );
        wait_for_build(&retry_gate).await;

        let duplicate = api.apply_task(manifest).await.unwrap();
        assert_eq!(duplicate.metadata().generation(), 1);
        assert_eq!(duplicate, retry);
        assert_eq!(builds.load(Ordering::Acquire), 2);

        retry_gate.release();
        wait_for_reconciled(&api, created.name(), 1, ConditionStatus::True).await;
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
        router.register(Arc::new(RecordingRunner { seen })).unwrap();
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
                api.delete_task_where(&name, WritePreconditions::new(), move |task| {
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
                api.apply_embedded_task(
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
        let stored = api.get_task(&name).expect("replacement remains stored");
        assert_eq!(stored.uid(), replacement.uid());
        assert_eq!(
            stored.metadata().generation(),
            replacement.metadata().generation()
        );
        assert_eq!(stored.spec(), replacement.spec());
        api.shutdown().await.unwrap();
    }

    struct BlockingRunner {
        gate: Arc<BuildGate>,
        runtime_started: Arc<AtomicBool>,
    }

    impl Runner for BlockingRunner {
        fn name(&self) -> &str {
            "blocking"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            subprocess_workload_types()
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            self.gate.started.store(true, Ordering::Release);
            let mut open = self.gate.open.lock();
            while !*open {
                self.gate.changed.wait(&mut open);
            }
            let runtime_started = Arc::clone(&self.runtime_started);
            Ok(TaskFn::arc(run_id.name(), move |_ctx: TaskContext| {
                runtime_started.store(true, Ordering::Release);
                async move { Ok::<(), TaskError>(()) }
            }))
        }
    }

    #[tokio::test]
    async fn desired_commit_returns_before_blocked_reconciliation() {
        let gate = Arc::new(BuildGate::new());
        let runtime_started = Arc::new(AtomicBool::new(false));
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(BlockingRunner {
                gate: Arc::clone(&gate),
                runtime_started: Arc::clone(&runtime_started),
            }))
            .unwrap();
        let api = api(router).await;
        let name = TaskId::new("detached-request").unwrap();

        let committed = tokio::time::timeout(
            Duration::from_millis(250),
            api.create_task(routed("detached-request", 1_000)),
        )
        .await
        .expect("desired commit must not wait for runner build")
        .unwrap();
        assert_eq!(committed.status().phase(), TaskPhase::Pending);
        assert_eq!(committed.status().observed_generation(), 0);
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
        fn name(&self) -> &str {
            "first-build-blocking"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            subprocess_workload_types()
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
                self.gate.started.store(true, Ordering::Release);
                let mut open = self.gate.open.lock();
                while !*open {
                    self.gate.changed.wait(&mut open);
                }
            }
            Ok(cancellable_task(run_id.name()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn newer_apply_reconciles_while_previous_preflight_is_blocked() {
        let gate = Arc::new(BuildGate::new());
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(FirstBuildBlockingRunner {
                gate: Arc::clone(&gate),
                builds: AtomicUsize::new(0),
            }))
            .unwrap();
        let api = api(router).await;
        let name = TaskId::new("latest-generation-wins").unwrap();

        let first = api
            .write(
                routed(name.as_str(), 1_000),
                RuntimeSource::Routed,
                WriteMode::Create,
                WritePreconditions::new(),
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
        assert_eq!(second.status().phase(), TaskPhase::Pending);

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
        router
            .register(Arc::new(BlockingRunner {
                gate: Arc::clone(&gate),
                runtime_started: Arc::clone(&runtime_started),
            }))
            .unwrap();
        let api = api(router).await;
        let name = TaskId::new("delete-before-bind").unwrap();

        let scheduled = api
            .write(
                routed(name.as_str(), 1_000),
                RuntimeSource::Routed,
                WriteMode::Create,
                WritePreconditions::new(),
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
            .create_embedded_task(
                embedded("too-late", 1_000),
                immediate_task("too-late-runtime"),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::ShuttingDown));
        assert!(api.get_task(&TaskId::new("too-late").unwrap()).is_none());
    }

    #[tokio::test]
    async fn shutdown_and_drop_close_task_watches() {
        let shutdown_api = api(RunnerRouter::new()).await;
        let mut shutdown_watch = shutdown_api
            .watch_tasks(&TaskFilter::new(), Some("0"))
            .unwrap();
        shutdown_api.shutdown().await.unwrap();
        assert!(shutdown_watch.next().await.is_none());

        let dropped_api = api(RunnerRouter::new()).await;
        let mut dropped_watch = dropped_api
            .watch_tasks(&TaskFilter::new(), Some("0"))
            .unwrap();
        drop(dropped_api);
        assert!(dropped_watch.next().await.is_none());
    }
}
