//! Supervisor API.
//!
//! [`SupervisorApi`] is the main entry point of `solti-core`.
//! It owns the taskvisor runtime, a runner router, in-memory state, and the
//! core-owned output hub used for live logs.
//!
//! ## State Reconstruction
//!
//! Task state is rebuilt from two paths with separate roles:
//!
//! - The event path (`StateSubscriber`, fed by taskvisor's best-effort event
//!   bus) owns per-attempt detail: phase transitions, `TaskRun` records, and
//!   output announcements.
//! - The direct completion path (`finalize_from_outcome`, fed by a per-submission
//!   `TaskWaiter`) is the terminal lifecycle authority. It releases identity and
//!   output resources after a `TaskRemoved` FIFO barrier normally lets earlier
//!   attempt events drain, with the direct outcome as a bounded fallback.
//!   Per-attempt detail remains best-effort. An unexpected waiter closure stays
//!   fail-closed until exact-identity cleanup is independently confirmed.
//!
//! The direct path reconciles the resource-level final disposition even when a
//! previous attempt event was terminal. Attempt history keeps its own outcome;
//! a concrete attempt `Timeout` remains more specific than the final task's generic
//! `Exhausted` outcome.
//! Both paths use the same phase crosswalk (`map::phase`) so they agree on the
//! final meaning.
use std::{
    collections::HashMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use solti_model::{Task, TaskId, TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec};
use taskvisor::{
    ControllerConfig, ControllerSpec, Subscribe, Supervisor, SupervisorConfig, SupervisorHandle,
    TaskRef, TaskSpec as TvTaskSpec,
};
use tokio_util::task::TaskTracker;
use tracing::{debug, info, instrument, warn};

use solti_runner::RunnerRouter;

use crate::{
    error::CoreError,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    output::{OutputConfig, OutputHub, OutputSubscription},
    state::{
        LifecycleGate, ReservationRollback, StateConfig, StateSubscriber, TaskState, state_sweep,
    },
};

/// High-level supervisor API.
///
/// `SupervisorApi` is the main entry point for host applications. It starts the
/// taskvisor runtime, submits tasks, exposes in-memory state, and shuts the
/// runtime down.
///
/// ## Example
///
/// ```rust,no_run
/// use solti_core::{CoreError, StateConfig, SupervisorApi};
/// use solti_runner::RunnerRouter;
/// use taskvisor::{ControllerConfig, SupervisorConfig};
///
/// async fn demo() -> Result<(), CoreError> {
///     let api = SupervisorApi::new(
///         SupervisorConfig::default(),
///         ControllerConfig::default(),
///         Vec::new(),
///         RunnerRouter::new(),
///         StateConfig::default(),
///     )
///     .await?;
///
///     assert!(api.list_all_tasks().is_empty());
///     api.shutdown().await?;
///     Ok(())
/// }
/// ```
pub struct SupervisorApi {
    output_hub: Arc<OutputHub>,
    handle: SupervisorHandle,
    router: RunnerRouter,
    state: TaskState,
    state_subscriber: Arc<StateSubscriber>,
    task_operations: TaskOperationLocks,
    lifecycle_gate: LifecycleGate,
    completion_runtime: tokio::runtime::Handle,
    completion_tasks: TaskTracker,
    #[cfg(test)]
    completion_spawn_hook:
        parking_lot::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    grace: Duration,
    shutdown_started: AtomicBool,
}

/// Per-task serialization for submit, cancel, and delete operations.
///
/// Weak entries avoid retaining one lock for every historical task id. Different
/// task ids still progress independently.
#[derive(Default)]
struct TaskOperationLocks {
    locks: parking_lot::Mutex<HashMap<TaskId, Weak<tokio::sync::Mutex<()>>>>,
}

impl TaskOperationLocks {
    async fn lock(&self, task_id: &TaskId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock();
            if let Some(lock) = locks.get(task_id).and_then(Weak::upgrade) {
                lock
            } else {
                locks.retain(|_, lock| lock.strong_count() > 0);
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(task_id.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

/// Rolls back the local half of a submission unless controller intake succeeds.
///
/// The guard stays armed across the bounded controller-queue wait. Dropping the
/// submit future before queue acceptance therefore cannot leave a bound
/// `Pending` entry or an output channel owned by that reservation.
struct SubmitReservation {
    state: TaskState,
    lifecycle_gate: LifecycleGate,
    task_id: TaskId,
    rollback: Option<ReservationRollback>,
    output_hub: Option<Arc<OutputHub>>,
}

impl SubmitReservation {
    fn new(
        state: TaskState,
        lifecycle_gate: LifecycleGate,
        task_id: TaskId,
        rollback: ReservationRollback,
    ) -> Self {
        Self {
            state,
            lifecycle_gate,
            task_id,
            rollback: Some(rollback),
            output_hub: None,
        }
    }

    fn track_output_channel(&mut self, output_hub: Arc<OutputHub>, created: bool) {
        if created {
            self.output_hub = Some(output_hub);
        }
    }

    fn commit(mut self) {
        self.rollback = None;
        self.output_hub = None;
    }
}

impl Drop for SubmitReservation {
    fn drop(&mut self) {
        if let Some(rollback) = self.rollback.take() {
            let _lifecycle = self.lifecycle_gate.lock();
            self.state.rollback_reservation(&self.task_id, rollback);
            if let Some(output_hub) = self.output_hub.take() {
                output_hub.evict(&self.task_id);
            }
        }
    }
}

impl Drop for SupervisorApi {
    fn drop(&mut self) {
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = self.handle.clone();
        let completion_tasks = self.completion_tasks.clone();
        completion_tasks.close();
        drop(self.completion_runtime.spawn(async move {
            let _ = handle.shutdown().await;
            completion_tasks.wait().await;
        }));
    }
}

impl SupervisorApi {
    /// Create a supervisor and start its run loop in the background.
    ///
    /// `StateSubscriber` and the periodic state sweep task are registered
    /// automatically. The supervisor creates the output hub and injects its
    /// producer capability into `router`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, StateConfig, SupervisorApi};
    /// use solti_runner::RunnerRouter;
    /// use taskvisor::{ControllerConfig, SupervisorConfig};
    ///
    /// async fn demo() -> Result<(), CoreError> {
    ///     let api = SupervisorApi::new(
    ///         SupervisorConfig::default(),
    ///         ControllerConfig::default(),
    ///         Vec::new(),
    ///         RunnerRouter::new(),
    ///         StateConfig::default(),
    ///     )
    ///     .await?;
    ///
    ///     api.shutdown().await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// Returns [`CoreError::Supervisor`] if taskvisor rejects the embedded sweep
    /// task during startup.
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

    /// Create a supervisor with explicit live-output configuration.
    ///
    /// The supervisor owns the concrete hub and injects only its producer
    /// capability into runners.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, OutputConfig, StateConfig, SupervisorApi};
    /// use solti_runner::RunnerRouter;
    /// use taskvisor::{ControllerConfig, SupervisorConfig};
    ///
    /// async fn demo() -> Result<(), CoreError> {
    ///     let api = SupervisorApi::new_with_output_config(
    ///         SupervisorConfig::default(),
    ///         ControllerConfig::default(),
    ///         Vec::new(),
    ///         RunnerRouter::new(),
    ///         StateConfig::default(),
    ///         OutputConfig::new(1024),
    ///     )
    ///     .await?;
    ///
    ///     api.shutdown().await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// Returns [`CoreError::Supervisor`] if taskvisor rejects the embedded sweep
    /// task during startup.
    pub async fn new_with_output_config(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
        output_config: OutputConfig,
    ) -> Result<Self, CoreError> {
        let output_hub = Arc::new(OutputHub::new(output_config));
        let router = router.with_output_publisher(output_hub.clone());
        let state = TaskState::new();
        state.set_max_runs_per_task(state_cfg.max_runs_per_task);
        let state_subscriber = Arc::new(StateSubscriber::with_output_hub(
            state.clone(),
            Arc::clone(&output_hub),
        ));
        let lifecycle_gate = state_subscriber.lifecycle_gate();
        subscribers.push(state_subscriber.clone());

        let grace = sup_cfg.grace();
        let completion_runtime = tokio::runtime::Handle::current();
        let sup = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();

        let handle = sup.serve();

        let api = Self {
            handle,
            router,
            state,
            state_subscriber,
            task_operations: TaskOperationLocks::default(),
            lifecycle_gate,
            output_hub,
            completion_runtime,
            completion_tasks: TaskTracker::new(),
            #[cfg(test)]
            completion_spawn_hook: parking_lot::Mutex::new(None),
            grace,
            shutdown_started: AtomicBool::new(false),
        };

        let (task, spec) = state_sweep(api.state.clone(), state_cfg);
        api.submit_with_task_inner(task, &spec, false).await?;
        info!("supervisor is ready (sweep active)");

        Ok(api)
    }

    /// Subscribe to one task's lossy, live-only output stream.
    ///
    /// Returns `None` when no active output lifecycle exists for `id`.
    pub fn subscribe_output(&self, id: &TaskId) -> Option<OutputSubscription> {
        self.output_hub.subscribe(id)
    }

    /// Return one task by id.
    pub fn get_task(&self, id: &TaskId) -> Option<Task> {
        self.state.get(id)
    }

    /// List every retained task in one slot.
    ///
    /// Unlike [`query_tasks`](Self::query_tasks), this low-level view also
    /// includes embedded or internal tasks when the caller names their slot.
    pub fn list_tasks_by_slot(&self, slot: &str) -> Vec<Task> {
        self.state.list_by_slot(slot)
    }

    /// List all public tasks.
    ///
    /// Internal Solti maintenance tasks are excluded.
    pub fn list_all_tasks(&self) -> Vec<Task> {
        self.state.list_all()
    }

    /// List public tasks by phase.
    pub fn list_tasks_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        self.state.list_by_status(phase)
    }

    /// Query public tasks with combined filters and pagination.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::SupervisorApi;
    /// use solti_model::{TaskPhase, TaskQuery};
    ///
    /// fn demo(api: &SupervisorApi) {
    ///     let query = TaskQuery::new()
    ///         .with_status(TaskPhase::Running)
    ///         .with_limit(20);
    ///     let page = api.query_tasks(&query);
    ///
    ///     assert!(page.items.len() <= 20);
    /// }
    /// ```
    pub fn query_tasks(&self, query: &TaskQuery) -> TaskPage<Task> {
        self.state.query(query)
    }

    /// List execution history for one task, oldest first.
    pub fn list_task_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        self.state.list_runs(id)
    }

    /// Stop a task (running **or** still queued in its slot) and purge its run history.
    ///
    /// Deleting a missing task is not an error: the call is an idempotent no-op.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, SupervisorApi};
    /// use solti_model::TaskId;
    ///
    /// async fn demo(api: &SupervisorApi, id: &TaskId) -> Result<(), CoreError> {
    ///     api.delete_task(id).await?;
    ///     assert!(api.get_task(id).is_none());
    ///     assert!(api.list_task_runs(id).is_empty());
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "cancel"`): the runtime failed to cancel the bound submission (registered or controller-queued).
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn delete_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("deleting task: {}", id);
        let _operation = self.task_operations.lock(id).await;

        let cancellation = self.cancel_bound(id).await?;
        let claimed_stop = cancellation.is_some_and(|(_, claimed)| claimed);
        let had_local = self
            .state_subscriber
            .delete_after_cleanup(id, cancellation.map(|(tv, _)| tv));

        if !claimed_stop && !had_local {
            debug!("delete_task: no such task in supervisor or state; idempotent no-op");
        }
        Ok(())
    }

    /// Cancel the taskvisor submission bound to `id`, covering both running and queued work.
    ///
    /// taskvisor's unified identity-based cancellation waits for terminal registry cleanup
    /// when the task is registered and directly removes controller-queued work before it starts.
    /// This confirmation path does not depend on best-effort lifecycle events;
    /// [`cancel_task`](Self::cancel_task) also settles the matching local binding
    /// before it returns.
    async fn cancel_bound(
        &self,
        id: &TaskId,
    ) -> Result<Option<(taskvisor::TaskId, bool)>, CoreError> {
        let Some(tv) = self.state.tv_for(id) else {
            return Ok(None);
        };

        let claimed = self
            .handle
            .cancel_with_timeout(tv, self.grace.saturating_add(Duration::from_secs(1)))
            .await
            .map_err(|e| CoreError::supervisor("cancel", e))?;
        Ok(Some((tv, claimed)))
    }

    /// Return a clone of the underlying taskvisor supervisor handle.
    ///
    /// Use this only when you need a taskvisor-specific operation that
    /// `SupervisorApi` does not wrap. In particular, prefer
    /// [`SupervisorApi::shutdown`] over calling shutdown on this raw handle:
    /// the SDK method also drains its tracked completion workers.
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Return a cheap clone of the shared [`TaskState`].
    ///
    /// Later supervisor updates are visible through the clone. This is useful
    /// for read-only consumers such as metrics collectors.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::SupervisorApi;
    /// use solti_model::TaskPhase;
    ///
    /// fn demo(api: &SupervisorApi) {
    ///     let state = api.state();
    ///     let counts = state.count_by_phase();
    ///     let running = counts.get(&TaskPhase::Running).copied().unwrap_or(0);
    ///     assert_eq!(running, state.list_by_status(TaskPhase::Running).len());
    /// }
    /// ```
    pub fn state(&self) -> TaskState {
        self.state.clone()
    }

    /// Build and submit a task described by [`TaskSpec`].
    ///
    /// Steps:
    /// 1. Ask the [`RunnerRouter`] to pick a runner and build a [`TaskRef`].
    /// 2. Delegate to [`SupervisorApi::submit_with_task`].
    ///
    /// This is the primary entrypoint for tasks that are fully described by the public [`solti_model::TaskKind`] model.
    /// The API must be created with a [`RunnerRouter`] that can build that task kind.
    ///
    /// `Ok(task_id)` confirms that taskvisor's bounded controller command queue
    /// accepted the submission and that Solti bound its local task resource to
    /// the reserved runtime identity. Slot admission, runtime registration, and
    /// the first task attempt happen asynchronously. Query the task to observe
    /// that result; a later admission rejection becomes a terminal state.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, SupervisorApi};
    /// use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec};
    ///
    /// async fn demo(api: &SupervisorApi) -> Result<(), CoreError> {
    ///     // `api` must use a router that supports `TaskKind::Subprocess`.
    ///     let kind = TaskKind::Subprocess(SubprocessSpec::new(
    ///         SubprocessMode::Command {
    ///             command: "true".into(),
    ///             args: vec![],
    ///         },
    ///         TaskEnv::default(),
    ///         None,
    ///         Flag::enabled(),
    ///     ));
    ///     let spec = TaskSpec::builder("checks", kind, 5_000_u64).build()?;
    ///
    ///     let task_id = api.submit(&spec).await?;
    ///     assert!(api.get_task(&task_id).is_some());
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`CoreError::InvalidSpec`]: the spec failed validation. This covers structural problems and `TaskKind::Embedded`, which cannot go through runners (use [`SupervisorApi::submit_with_task`]).
    /// - [`CoreError::Runner`]: no registered runner matches the spec's kind/selector, or the runner failed to build the task.
    /// - [`CoreError::Mapping`]: a spec policy has no taskvisor equivalent (unknown `#[non_exhaustive]` variant), or the backoff parameters are invalid.
    /// - [`CoreError::AlreadyExists`]: a live submission still owns the same task id, including a bound task between attempts.
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the controller was unavailable or closed before queue intake; the provisional state entry is rolled back.
    #[instrument(level = "debug", skip(self, spec), fields(slot = %spec.slot(), kind = ?spec.kind()))]
    pub async fn submit(&self, spec: &TaskSpec) -> Result<TaskId, CoreError> {
        spec.validate()?;

        let task = self.router.build(spec)?;
        self.submit_with_task(task, spec).await
    }

    /// Submit a pre-built task together with its spec.
    ///
    /// This API is intended for in-process / code-defined tasks (with `TaskKind::Embedded`).
    ///
    /// The caller is responsible for constructing the [`TaskRef`];
    /// the spec controls timeout, restart, backoff and admission behavior.
    ///
    /// `Ok(task_id)` confirms controller-queue intake and local identity binding,
    /// not slot admission, runtime registration, or task-body start. Those happen
    /// asynchronously. A later admission rejection is delivered through the
    /// direct completion path and reflected in task state.
    /// A successful binding pre-creates this task's live-tail output channel;
    /// task bodies should acquire [`solti_runner::OutputSink`] values lazily per
    /// attempt rather than capturing them before submission.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, SupervisorApi};
    /// use solti_model::{RestartPolicy, TaskKind, TaskSpec};
    /// use taskvisor::{TaskContext, TaskError, TaskFn};
    ///
    /// async fn demo(api: &SupervisorApi) -> Result<(), CoreError> {
    ///     let task = TaskFn::arc("embedded-once", |_ctx: TaskContext| async move {
    ///         Ok::<(), TaskError>(())
    ///     });
    ///     let spec = TaskSpec::builder("embedded", TaskKind::Embedded, 1_000_u64)
    ///         .restart(RestartPolicy::Never)
    ///         .build()?;
    ///
    ///     let task_id = api.submit_with_task(task, &spec).await?;
    ///     assert!(api.get_task(&task_id).is_some());
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`CoreError::InvalidSpec`]: the task name fails `TaskId` format validation.
    /// - [`CoreError::Mapping`]: a spec policy has no taskvisor equivalent (unknown `#[non_exhaustive]` variant), or the backoff parameters are invalid.
    /// - [`CoreError::AlreadyExists`]: a live submission still owns the same task id, including a bound task between attempts.
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the controller was unavailable or closed before queue intake; the provisional state entry is rolled back.
    #[instrument(level = "debug", skip(self, task, spec), fields(slot = %spec.slot()))]
    pub async fn submit_with_task(
        &self,
        task: TaskRef,
        spec: &TaskSpec,
    ) -> Result<TaskId, CoreError> {
        self.submit_with_task_inner(task, spec, true).await
    }

    async fn submit_with_task_inner(
        &self,
        task: TaskRef,
        spec: &TaskSpec,
        ensure_output: bool,
    ) -> Result<TaskId, CoreError> {
        let task_id = TaskId::from(task.name());
        task_id.validate_format()?;

        let task_spec = TvTaskSpec::new(
            task,
            to_restart_policy(spec.restart())?,
            to_backoff_policy(spec.backoff())?,
            Some(Duration::from_millis(spec.timeout().as_millis())),
        )
        .with_max_retries(spec.max_retries());
        let controller_spec =
            ControllerSpec::new(to_admission_policy(spec.admission())?, task_spec)
                .with_slot(spec.slot().as_str());

        // Keep same-id management atomic across local reservation, prepared
        // runtime-identity binding, and bounded controller intake. The
        // reservation guard is declared after this lock, so cancellation
        // drops/rolls it back before another same-id operation can enter.
        let _operation = self.task_operations.lock(&task_id).await;
        let rollback = {
            let _lifecycle = self.lifecycle_gate.lock();
            self.state.reserve(task_id.clone(), spec.clone())?
        };
        let mut reservation = SubmitReservation::new(
            self.state.clone(),
            self.lifecycle_gate.clone(),
            task_id.clone(),
            rollback,
        );

        let prepared = self
            .handle
            .prepare_submission(controller_spec)
            .map_err(|error| CoreError::supervisor("submit", error))?;
        let tv_id = prepared.id();
        let output_created = self.state_subscriber.bind(&task_id, tv_id, ensure_output);
        reservation.track_output_channel(Arc::clone(&self.output_hub), output_created);

        debug!("submitting pre-built task via controller");
        // A shutdown closes and waits this tracker after taskvisor has stopped.
        // Hold a registration across controller intake until the durable waiter
        // worker is itself tracked, so concurrent shutdown cannot observe an
        // empty tracker in the hand-off window.
        let completion_registration = self.completion_tasks.token();
        match prepared.submit_and_watch().await {
            Ok((submitted_tv_id, waiter)) => {
                debug_assert_eq!(submitted_tv_id, tv_id);
                #[cfg(test)]
                if let Some((arrived, release)) = { self.completion_spawn_hook.lock().clone() } {
                    arrived.notify_one();
                    release.notified().await;
                }

                reservation.commit();

                let state_subscriber = Arc::clone(&self.state_subscriber);
                let cleanup_handle = self.handle.clone();
                let cleanup_timeout = self.grace.saturating_add(Duration::from_secs(1));
                let tv_raw = tv_id.get();
                let completion_task = self.completion_tasks.spawn_on(
                    async move {
                        match waiter.wait().await {
                        Ok(outcome) => {
                            state_subscriber
                                .finalize_from_outcome(tv_raw, &outcome)
                                .await;
                        }
                        Err(error) => {
                            warn!(
                                taskvisor_id = tv_raw,
                                error = %error,
                                "task completion channel closed without an outcome",
                            );
                            let unavailable = format!("task outcome unavailable: {error}");
                            match cleanup_handle
                                .cancel_with_timeout(tv_id, cleanup_timeout)
                                .await
                            {
                                Ok(_) => {
                                    state_subscriber
                                        .finalize_unavailable_after_cleanup(tv_raw, unavailable)
                                        .await;
                                }
                                Err(cleanup_error) => {
                                    warn!(
                                        taskvisor_id = tv_raw,
                                        error = %cleanup_error,
                                        "could not confirm cleanup after task outcome became unavailable",
                                    );
                                    state_subscriber.finalize_unavailable(tv_raw, unavailable);
                                }
                            }
                        }
                    }
                    },
                    &self.completion_runtime,
                );
                drop(completion_task);
                drop(completion_registration);

                Ok(task_id)
            }
            Err(e) => Err(CoreError::supervisor("submit", e)),
        }
    }

    /// Gracefully shut down the supervisor.
    ///
    /// This asks taskvisor to stop all tasks, waits for runtime shutdown, and
    /// drains SDK-owned completion workers. It borrows the API, so callers may
    /// use it through an [`Arc`].
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, SupervisorApi};
    ///
    /// async fn demo(api: SupervisorApi) -> Result<(), CoreError> {
    ///     api.shutdown().await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "shutdown"`): the runtime failed to stop cleanly (for example, tasks had to be force-aborted after the grace period).
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        info!("initiating graceful shutdown");
        let res = self
            .handle
            .clone()
            .shutdown()
            .await
            .map_err(|e| CoreError::supervisor("shutdown", e));
        self.completion_tasks.close();
        self.completion_tasks.wait().await;
        self.shutdown_started.store(true, Ordering::Release);
        res
    }

    /// Cancel a task by ID, whether it is running or still queued.
    ///
    /// Unlike [`delete_task`](Self::delete_task), this keeps already recorded
    /// run history until normal retention cleanup.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_core::{CoreError, SupervisorApi};
    /// use solti_model::TaskId;
    ///
    /// async fn demo(api: &SupervisorApi, id: &TaskId) -> Result<(), CoreError> {
    ///     api.cancel_task(id).await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`CoreError::NotFound`]: nothing was cancelled and no state entry exists for `id`.
    /// - [`CoreError::Supervisor`] (`op = "cancel"`): the runtime failed to cancel the bound submission (registered or controller-queued).
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn cancel_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("cancelling task: {}", id);
        let _operation = self.task_operations.lock(id).await;

        // `false` can also mean another caller already claimed the stop and we
        // joined its cleanup. Snapshot local knowledge before that cleanup can
        // remove the state entry.
        let was_known = self.state.get(id).is_some();
        let cancellation = self.cancel_bound(id).await?;
        let claimed_stop = cancellation.as_ref().is_some_and(|(_, claimed)| *claimed);
        if let Some((tv, _)) = cancellation {
            self.state_subscriber
                .settle_after_confirmed_cleanup(tv)
                .await;
        }
        if !claimed_stop && !was_known {
            return Err(CoreError::NotFound(id.to_string()));
        }

        debug!("task cancellation issued: {}", id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    use solti_model::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind};
    use taskvisor::{TaskContext, TaskError, TaskFn};

    fn mk_backoff() -> BackoffPolicy {
        BackoffPolicy {
            jitter: JitterPolicy::Equal,
            first_ms: 1_000,
            max_ms: 5_000,
            factor: 2.0,
        }
    }

    #[tokio::test]
    async fn max_retries_budget_reaches_taskvisor() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let task: TaskRef = TaskFn::arc("budget-task", |_ctx: TaskContext| async move {
            Err::<(), TaskError>(TaskError::fail("always fails"))
        });

        let spec = TaskSpec::builder("budget-slot", TaskKind::Embedded, 5_000_u64)
            .restart(RestartPolicy::OnFailure)
            .backoff(solti_model::BackoffPolicy {
                jitter: solti_model::JitterPolicy::None,
                first_ms: 1,
                max_ms: 1,
                factor: 1.0,
            })
            .max_retries(std::num::NonZeroU32::new(2))
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");

        let task_id = api.submit_with_task(task, &spec).await.expect("submit ok");

        let mut settled = false;
        for _ in 0..200 {
            let runs = api.list_task_runs(&task_id);
            if runs.len() >= 3 && runs.iter().all(|r| !r.is_active()) {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(settled, "task must stop retrying after its budget");

        tokio::time::sleep(Duration::from_millis(150)).await;
        let runs = api.list_task_runs(&task_id);
        assert_eq!(
            runs.len(),
            3,
            "retry budget of 2 means exactly 3 attempts, got {}",
            runs.len()
        );
        assert!(runs.iter().all(|run| run.phase == TaskPhase::Failed));
    }

    #[tokio::test]
    async fn fresh_supervisor_lists_no_internal_tasks() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        assert!(
            api.list_all_tasks().is_empty(),
            "the embedded sweep task must not appear in list_all_tasks()"
        );
        assert_eq!(
            api.query_tasks(&solti_model::TaskQuery::new()).total,
            0,
            "the embedded sweep task must not appear in a slot-less query"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_name_submits_yield_one_ok_one_already_exists() {
        let api = Arc::new(
            SupervisorApi::new(
                SupervisorConfig::default(),
                ControllerConfig::default(),
                Vec::new(),
                RunnerRouter::new(),
                StateConfig::default(),
            )
            .await
            .expect("SupervisorApi::new"),
        );

        fn long_task() -> TaskRef {
            TaskFn::arc("dup-name", |ctx: TaskContext| async move {
                while !ctx.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok::<(), TaskError>(())
            })
        }
        let spec = TaskSpec::builder("dup-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("spec builds");

        let a = {
            let api = Arc::clone(&api);
            let spec = spec.clone();
            tokio::spawn(async move { api.submit_with_task(long_task(), &spec).await })
        };
        let b = {
            let api = Arc::clone(&api);
            let spec = spec.clone();
            tokio::spawn(async move { api.submit_with_task(long_task(), &spec).await })
        };
        let ra = a.await.unwrap();
        let rb = b.await.unwrap();

        let oks = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
        let already = [&ra, &rb]
            .iter()
            .filter(|r| matches!(r, Err(CoreError::AlreadyExists(_))))
            .count();
        assert_eq!(
            oks, 1,
            "exactly one same-name submit may win the reservation"
        );
        assert_eq!(
            already, 1,
            "the racing submit must be rejected as AlreadyExists, not orphan a binding"
        );
    }

    #[tokio::test]
    async fn submit_with_task_succeeds_for_simple_task() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .expect("failed to create SupervisorApi");

        let task: TaskRef = TaskFn::arc("test-task", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });

        let spec = TaskSpec::builder("test-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");

        let task_id = api
            .submit_with_task(task, &spec)
            .await
            .expect("expected Ok(TaskId)");
        assert!(!task_id.as_str().is_empty());
        assert!(task_id.as_str().contains("test-task"));

        let mut terminal = false;
        for _ in 0..100 {
            terminal = api
                .get_task(&task_id)
                .is_some_and(|task| task.status().phase.is_terminal());
            if terminal {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(terminal, "the one-shot task must reach a terminal phase");
        tokio::task::yield_now().await;
        assert!(
            api.get_task(&task_id).is_some(),
            "TaskRemoved must not bypass task_ttl retention"
        );
    }

    #[tokio::test]
    async fn delete_task_stops_running_task_and_wipes_state() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let cancelled_observed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled_observed);
        let task: TaskRef = TaskFn::arc("kill-me", move |ctx: TaskContext| {
            let flag = Arc::clone(&flag);
            async move {
                while !ctx.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                flag.store(true, Ordering::SeqCst);
                Ok::<(), TaskError>(())
            }
        });

        let spec = TaskSpec::builder("slot-delete", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::Replace)
            .build()
            .expect("spec builds");

        let task_id = api
            .submit_with_task(task, &spec)
            .await
            .expect("submit_with_task");

        let handle = api.handle();
        let mut alive = false;
        for _ in 0..100 {
            if handle.is_alive(task_id.as_str()).await {
                alive = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            alive,
            "task body must reach Running state before we try to delete"
        );
        api.output_hub.ensure_channel(task_id.clone());
        assert!(api.output_hub.subscribe_raw(&task_id).is_some());

        api.delete_task(&task_id)
            .await
            .expect("delete_task must Ok");

        assert!(
            api.get_task(&task_id).is_none(),
            "state must be wiped after delete"
        );
        assert!(
            api.list_task_runs(&task_id).is_empty(),
            "run history must be purged by delete"
        );
        assert!(
            api.output_hub.subscribe_raw(&task_id).is_none(),
            "explicit delete must evict the output channel"
        );

        for _ in 0..100 {
            if cancelled_observed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            cancelled_observed.load(Ordering::SeqCst),
            "task body must observe the cancel token - delete must cancel, not just wipe state"
        );
    }

    #[tokio::test]
    async fn cancel_task_removes_queued_submission_by_identity() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let owner_started = Arc::new(AtomicBool::new(false));
        let owner_flag = Arc::clone(&owner_started);
        let owner: TaskRef = TaskFn::arc("queue-owner", move |ctx: TaskContext| {
            let owner_flag = Arc::clone(&owner_flag);
            async move {
                owner_flag.store(true, Ordering::SeqCst);
                ctx.cancelled().await;
                Ok::<(), TaskError>(())
            }
        });
        let owner_spec = TaskSpec::builder("identity-cancel-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("owner spec builds");
        let owner_id = api
            .submit_with_task(owner, &owner_spec)
            .await
            .expect("owner submit");

        for _ in 0..100 {
            if owner_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            owner_started.load(Ordering::SeqCst),
            "slot owner must start before the queued submission"
        );

        let queued_started = Arc::new(AtomicBool::new(false));
        let queued_flag = Arc::clone(&queued_started);
        let queued: TaskRef = TaskFn::arc("queue-tail", move |_ctx: TaskContext| {
            let queued_flag = Arc::clone(&queued_flag);
            async move {
                queued_flag.store(true, Ordering::SeqCst);
                Ok::<(), TaskError>(())
            }
        });
        let queued_spec = TaskSpec::builder("identity-cancel-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::Queue)
            .build()
            .expect("queued spec builds");
        let queued_id = api
            .submit_with_task(queued, &queued_spec)
            .await
            .expect("queued submit");

        api.cancel_task(&queued_id)
            .await
            .expect("queued cancellation must be accepted");

        let mut phase = None;
        for _ in 0..100 {
            phase = api.get_task(&queued_id).map(|task| task.status().phase);
            if phase == Some(TaskPhase::Canceled) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(phase, Some(TaskPhase::Canceled));
        assert!(
            !queued_started.load(Ordering::SeqCst),
            "identity cancellation must remove queued work before its body starts"
        );

        let queued_again: TaskRef = TaskFn::arc("queue-tail", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let queued_again_id = api
            .submit_with_task(queued_again, &queued_spec)
            .await
            .expect("cancel must release the local identity before returning");
        assert_eq!(queued_again_id, queued_id);
        api.delete_task(&queued_again_id)
            .await
            .expect("delete resubmitted queued task");

        api.delete_task(&owner_id).await.expect("delete owner");
    }

    #[tokio::test]
    async fn cancel_running_task_allows_immediate_same_id_resubmit() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let started = Arc::new(AtomicBool::new(false));
        let started_by_task = Arc::clone(&started);
        let first: TaskRef = TaskFn::arc("cancel-resubmit", move |ctx: TaskContext| {
            let started = Arc::clone(&started_by_task);
            async move {
                started.store(true, Ordering::SeqCst);
                ctx.cancelled().await;
                Ok::<(), TaskError>(())
            }
        });
        let spec = TaskSpec::builder("cancel-resubmit-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("spec builds");
        let id = api
            .submit_with_task(first, &spec)
            .await
            .expect("first submit");

        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(started.load(Ordering::SeqCst), "first task must start");

        api.cancel_task(&id).await.expect("cancel running task");
        assert_eq!(
            api.get_task(&id).map(|task| task.status().phase),
            Some(TaskPhase::Canceled),
            "the task-level canceled outcome must reconcile a successful attempt body"
        );
        assert_eq!(
            api.list_task_runs(&id)[0].phase,
            TaskPhase::Succeeded,
            "attempt history keeps the body result"
        );
        let second: TaskRef = TaskFn::arc("cancel-resubmit", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let second_id = api
            .submit_with_task(second, &spec)
            .await
            .expect("cancel must release the local identity before returning");
        assert_eq!(second_id, id);

        api.delete_task(&second_id)
            .await
            .expect("delete resubmitted task");
    }

    #[test]
    fn completion_waiter_remains_owned_by_the_supervisor_runtime() {
        let supervisor_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("supervisor runtime");
        let api = supervisor_runtime
            .block_on(SupervisorApi::new(
                SupervisorConfig::default(),
                ControllerConfig::default(),
                Vec::new(),
                RunnerRouter::new(),
                StateConfig::default(),
            ))
            .expect("SupervisorApi::new");

        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let task_release = Arc::clone(&release);
        let task: TaskRef = TaskFn::arc("cross-runtime-waiter", move |_ctx: TaskContext| {
            let release = Arc::clone(&task_release);
            async move {
                let _permit = release.acquire_owned().await.expect("semaphore open");
                Ok::<(), TaskError>(())
            }
        });
        let spec = TaskSpec::builder("cross-runtime-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .build()
            .expect("spec builds");

        let submit_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("submit runtime");
        let (api, id) = submit_runtime.block_on(async move {
            let id = api
                .submit_with_task(task, &spec)
                .await
                .expect("cross-runtime submit");
            (api, id)
        });
        drop(submit_runtime);

        release.add_permits(1);
        supervisor_runtime.block_on(async move {
            for _ in 0..200 {
                if api.state.tv_for(&id).is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            assert!(
                api.state.tv_for(&id).is_none(),
                "dropping the submit runtime must not abort completion cleanup"
            );
            assert_eq!(
                api.get_task(&id).map(|task| task.status().phase),
                Some(TaskPhase::Succeeded)
            );
            api.shutdown().await.expect("shutdown");
        });
    }

    #[tokio::test]
    async fn shutdown_drains_tracked_completion_tasks() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");
        let task: TaskRef = TaskFn::arc("shutdown-drain", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok::<(), TaskError>(())
        });
        let spec = TaskSpec::builder("shutdown-drain-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .build()
            .expect("spec builds");
        let id = api.submit_with_task(task, &spec).await.expect("submit");
        let state = api.state();
        let completion_tasks = api.completion_tasks.clone();

        api.shutdown().await.expect("shutdown");

        assert!(completion_tasks.is_closed());
        assert!(completion_tasks.is_empty());
        assert!(state.tv_for(&id).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_waits_for_submit_to_register_its_completion_worker() {
        let api = Arc::new(
            SupervisorApi::new(
                SupervisorConfig::default(),
                ControllerConfig::default(),
                Vec::new(),
                RunnerRouter::new(),
                StateConfig::default(),
            )
            .await
            .expect("SupervisorApi::new"),
        );
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *api.completion_spawn_hook.lock() = Some((Arc::clone(&arrived), Arc::clone(&release)));

        let task: TaskRef =
            TaskFn::arc("shutdown-submit-handoff", |_ctx: TaskContext| async move {
                Ok::<(), TaskError>(())
            });
        let spec = TaskSpec::builder(
            "shutdown-submit-handoff-slot",
            TaskKind::Embedded,
            60_000_u64,
        )
        .restart(RestartPolicy::Never)
        .build()
        .expect("spec builds");
        let submit = {
            let api = Arc::clone(&api);
            tokio::spawn(async move { api.submit_with_task(task, &spec).await })
        };

        arrived.notified().await;
        let shutdown = {
            let api = Arc::clone(&api);
            tokio::spawn(async move { api.shutdown().await })
        };

        for _ in 0..200 {
            if api.completion_tasks.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            api.completion_tasks.is_closed(),
            "shutdown must reach its completion-worker drain"
        );
        assert!(
            !shutdown.is_finished(),
            "shutdown must wait across the submit-to-worker hand-off"
        );

        release.notify_one();
        let id = submit.await.expect("submit task").expect("submit result");
        shutdown
            .await
            .expect("shutdown task")
            .expect("shutdown result");

        assert!(api.completion_tasks.is_empty());
        assert!(api.state.tv_for(&id).is_none());
    }

    #[tokio::test]
    async fn dropping_api_without_shutdown_cancels_running_tasks() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let task: TaskRef = TaskFn::arc("drop-cancel", move |ctx: TaskContext| {
            let flag = Arc::clone(&flag);
            async move {
                while !ctx.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                flag.store(true, Ordering::SeqCst);
                Ok::<(), TaskError>(())
            }
        });
        let spec = TaskSpec::builder("drop-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::Replace)
            .build()
            .expect("spec builds");

        api.submit_with_task(task, &spec)
            .await
            .expect("submit_with_task");

        let handle = api.handle();
        for _ in 0..200 {
            if handle.is_alive("drop-cancel").await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        drop(api);

        let mut observed = false;
        for _ in 0..200 {
            if cancelled.load(Ordering::SeqCst) {
                observed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            observed,
            "dropping SupervisorApi without shutdown() must cancel running tasks (best-effort), not leak them"
        );
    }

    #[tokio::test]
    async fn submitting_an_active_duplicate_name_returns_already_exists() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let long = |name: &'static str| -> TaskRef {
            TaskFn::arc(name, |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Ok::<(), TaskError>(())
            })
        };
        let spec = TaskSpec::builder("dup-slot", TaskKind::Embedded, 60_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::Replace)
            .build()
            .expect("spec builds");

        let id = api
            .submit_with_task(long("dup-name"), &spec)
            .await
            .expect("first submit ok");

        let handle = api.handle();
        for _ in 0..200 {
            if handle.is_alive(id.as_str()).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let err = api
            .submit_with_task(long("dup-name"), &spec)
            .await
            .expect_err("duplicate active name must be rejected");
        assert!(
            matches!(err, CoreError::AlreadyExists(_)),
            "expected AlreadyExists, got {err:?}"
        );

        let _ = api.shutdown().await;
    }

    #[tokio::test]
    async fn submit_with_task_rejects_malformed_task_name() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let task: TaskRef = TaskFn::arc("bad name with spaces", |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        });
        let spec = TaskSpec::builder("ok-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("spec builds");

        let err = api
            .submit_with_task(task, &spec)
            .await
            .expect_err("malformed task name must be rejected");
        assert!(
            matches!(err, CoreError::InvalidSpec(_)),
            "expected InvalidSpec, got {err:?}"
        );

        let _ = api.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_missing_task_returns_not_found() {
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            RunnerRouter::new(),
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let err = api
            .cancel_task(&TaskId::from("never-existed"))
            .await
            .expect_err("cancel on missing must error");
        assert!(
            matches!(err, CoreError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );

        let _ = api.shutdown().await;
    }

    #[tokio::test]
    async fn delete_task_is_idempotent_on_missing() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        let missing = TaskId::from("never-submitted");
        api.delete_task(&missing)
            .await
            .expect("delete on missing id must be Ok");
    }

    #[tokio::test]
    async fn submit_rejects_taskkind_embedded() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .expect("failed to create SupervisorApi");

        let spec = TaskSpec::builder("test-slot-none", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");
        let res = api.submit(&spec).await;

        match res {
            Err(CoreError::InvalidSpec(e)) => {
                assert!(e.to_string().contains("TaskKind::Embedded"));
            }
            Ok(_) => panic!("expected error for TaskKind::Embedded, got Ok(TaskId)"),
            Err(e) => panic!("expected CoreError::InvalidSpec, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn supervisor_api_default_new_creates_empty_output_hub() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
        )
        .await
        .expect("SupervisorApi::new");

        assert_eq!(api.output_hub.active_channels(), 0);
        assert_eq!(api.output_hub.capacity(), OutputConfig::DEFAULT_CAPACITY);
    }

    #[tokio::test]
    async fn supervisor_api_uses_explicit_output_config() {
        let router = RunnerRouter::new();

        let api = SupervisorApi::new_with_output_config(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
            OutputConfig::new(64),
        )
        .await
        .expect("SupervisorApi::new_with_output_config");

        assert_eq!(api.output_hub.capacity(), 64);
        assert_eq!(api.output_hub.active_channels(), 0);
    }

    #[test]
    fn submit_reservation_drop_restores_state_and_preserves_external_output_channel() {
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        let state = TaskState::new();
        let ghost = TaskId::from("orphan-on-submit-fail");
        registry.ensure_channel(ghost.clone());
        let spec = TaskSpec::builder("ghost-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");
        let rollback = state.reserve(ghost.clone(), spec).expect("reserve state");
        let tv_id = taskvisor::TaskId::for_tests();
        state.bind_tv(&ghost, tv_id);
        let output_created = registry.ensure_channel_if_absent(ghost.clone());

        let channels_before = registry.active_channels();
        assert!(channels_before >= 1, "channel must exist before rollback");
        assert!(state.get(&ghost).is_some(), "state entry must exist");

        let mut reservation = SubmitReservation::new(
            state.clone(),
            LifecycleGate::default(),
            ghost.clone(),
            rollback,
        );
        reservation.track_output_channel(Arc::clone(&registry), output_created);
        drop(reservation);
        assert_eq!(
            registry.active_channels(),
            channels_before,
            "rollback must not detach a channel or sink created outside the reservation"
        );
        assert!(registry.subscribe_raw(&ghost).is_some());
        assert!(
            state.get(&ghost).is_none(),
            "state entry must be gone after rollback"
        );
        assert!(state.resolve_tv(tv_id.get()).is_none());
    }

    #[test]
    fn submit_reservation_drop_removes_owned_binding_and_output_channel() {
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        let state = TaskState::new();
        let task_id = TaskId::from("owned-submit-resources");
        let spec = TaskSpec::builder("owned-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .build()
            .expect("valid spec");
        let rollback = state.reserve(task_id.clone(), spec).expect("reserve state");
        let tv_id = taskvisor::TaskId::for_tests();
        state.bind_tv(&task_id, tv_id);
        let output_created = registry.ensure_channel_if_absent(task_id.clone());
        assert!(output_created);

        let mut reservation = SubmitReservation::new(
            state.clone(),
            LifecycleGate::default(),
            task_id.clone(),
            rollback,
        );
        reservation.track_output_channel(Arc::clone(&registry), output_created);
        drop(reservation);

        assert!(state.get(&task_id).is_none());
        assert!(state.resolve_tv(tv_id.get()).is_none());
        assert!(registry.subscribe_raw(&task_id).is_none());
    }

    #[test]
    fn committed_submit_reservation_keeps_state_entry_and_output_channel() {
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        let state = TaskState::new();
        let task_id = TaskId::from("accepted-submit");
        let spec = TaskSpec::builder("accepted-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");
        let rollback = state.reserve(task_id.clone(), spec).expect("reserve state");
        let tv_id = taskvisor::TaskId::for_tests();
        state.bind_tv(&task_id, tv_id);
        let output_created = registry.ensure_channel_if_absent(task_id.clone());

        let mut reservation = SubmitReservation::new(
            state.clone(),
            LifecycleGate::default(),
            task_id.clone(),
            rollback,
        );
        reservation.track_output_channel(Arc::clone(&registry), output_created);
        reservation.commit();

        assert!(state.get(&task_id).is_some());
        assert_eq!(state.tv_for(&task_id), Some(tv_id));
        assert_eq!(registry.active_channels(), 1);
    }

    #[test]
    fn submit_reservation_rollback_restores_retained_terminal_resource() {
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        let state = TaskState::new();
        let task_id = TaskId::from("retained-before-failed-submit");
        let old_spec = TaskSpec::builder("old-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .build()
            .expect("valid old spec");
        state.add_task(task_id.clone(), old_spec);
        state.transition_starting(&task_id);
        state.transition_finished(
            &task_id,
            TaskPhase::Failed,
            Some("old failure".into()),
            Some(17),
        );
        let old_task = state.get(&task_id).expect("retained task");
        let old_runs = state.list_runs(&task_id);

        registry.ensure_channel(task_id.clone());
        let new_spec = TaskSpec::builder("new-slot", TaskKind::Embedded, 2_000_u64)
            .restart(RestartPolicy::Never)
            .build()
            .expect("valid new spec");
        let rollback = state
            .reserve(task_id.clone(), new_spec)
            .expect("terminal task id is reusable");

        drop(SubmitReservation::new(
            state.clone(),
            LifecycleGate::default(),
            task_id.clone(),
            rollback,
        ));

        assert_eq!(state.get(&task_id), Some(old_task));
        assert_eq!(state.list_runs(&task_id), old_runs);
        assert_eq!(state.list_by_slot("old-slot").len(), 1);
        assert!(state.list_by_slot("new-slot").is_empty());
        assert!(state.tv_for(&task_id).is_none());
        assert!(registry.subscribe_raw(&task_id).is_some());
    }

    #[tokio::test]
    async fn task_operation_locks_serialize_only_the_same_task_id() {
        let locks = TaskOperationLocks::default();
        let first_id = TaskId::from("locked-task");
        let other_id = TaskId::from("independent-task");
        let first = locks.lock(&first_id).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(50), locks.lock(&other_id))
                .await
                .is_ok(),
            "different task ids must not block each other"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), locks.lock(&first_id))
                .await
                .is_err(),
            "the same task id must remain serialized while its operation is active"
        );

        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), locks.lock(&first_id))
                .await
                .is_ok(),
            "the next same-id operation must proceed after release"
        );
    }

    use taskvisor::TaskOutcome;

    fn bound_running_state(name: &str) -> (TaskState, TaskId, u64) {
        let state = TaskState::new();
        let id = TaskId::from(name);
        let spec = TaskSpec::builder(name, TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("valid spec");
        state.add_task(id.clone(), spec);
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);
        (state, id, tv.get())
    }

    fn finalize_from_outcome(
        state: &TaskState,
        registry: &Arc<OutputHub>,
        tv_raw: u64,
        outcome: &TaskOutcome,
    ) {
        StateSubscriber::with_output_hub(state.clone(), Arc::clone(registry))
            .finalize_outcome_immediately_for_test(tv_raw, outcome);
    }

    #[test]
    fn backstop_finalizes_a_lost_terminal_outcome() {
        let (state, id, tv_raw) = bound_running_state("lost-1");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));

        finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
    }

    #[test]
    fn backstop_evicts_output_channel_on_terminal_outcome() {
        let (state, id, tv_raw) = bound_running_state("leak-1");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        registry.ensure_channel(id.clone());
        assert!(
            registry.subscribe_raw(&id).is_some(),
            "channel exists before"
        );

        finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
        assert!(
            registry.subscribe_raw(&id).is_none(),
            "the backstop must evict the output channel on a terminal outcome"
        );
    }

    #[test]
    fn backstop_reconciles_a_stale_attempt_terminal_with_completed_outcome() {
        let (state, id, tv_raw) = bound_running_state("done-1");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), Some(1));

        finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Succeeded,
            "the joined task outcome is authoritative for the resource"
        );
        assert_eq!(state.list_runs(&id)[0].phase, TaskPhase::Failed);
    }

    #[test]
    fn backstop_reconciles_same_phase_diagnostics_from_panicked_outcome() {
        let (state, id, tv_raw) = bound_running_state("panic-diagnostics");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        state.transition_finished(&id, TaskPhase::Failed, Some("actor_panic".into()), Some(1));

        finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Panicked);

        let task = state.get(&id).expect("retained terminal task");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().error.as_deref(), Some("actor panicked"));
        assert_eq!(task.status().exit_code, None);
        assert_eq!(
            state.list_runs(&id)[0].error.as_deref(),
            Some("actor_panic")
        );
    }

    #[test]
    fn backstop_refines_failed_attempt_to_exhausted_actor_outcome() {
        let (state, id, tv_raw) = bound_running_state("exhausted-after-attempt");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        state.transition_finished(
            &id,
            TaskPhase::Failed,
            Some("attempt failed".into()),
            Some(1),
        );

        finalize_from_outcome(
            &state,
            &registry,
            tv_raw,
            &TaskOutcome::failed_for_tests("max retries exceeded", Some(1)),
        );

        let task = state.get(&id).expect("retained terminal task");
        assert_eq!(task.status().phase, TaskPhase::Exhausted);
        assert_eq!(task.status().error.as_deref(), Some("max retries exceeded"));
        assert!(state.tv_for(&id).is_none());
        assert_eq!(state.list_runs(&id)[0].phase, TaskPhase::Failed);
    }

    #[test]
    fn backstop_preserves_timeout_over_generic_exhausted_outcome() {
        let (state, id, tv_raw) = bound_running_state("timeout-before-exhaustion");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        state.transition_finished(
            &id,
            TaskPhase::Timeout,
            Some("attempt timed out".into()),
            None,
        );

        finalize_from_outcome(
            &state,
            &registry,
            tv_raw,
            &TaskOutcome::failed_for_tests("max retries exceeded", None),
        );

        let task = state.get(&id).expect("retained terminal task");
        assert_eq!(task.status().phase, TaskPhase::Timeout);
        assert_eq!(task.status().error.as_deref(), Some("attempt timed out"));
        assert!(state.tv_for(&id).is_none());
    }

    #[test]
    fn backstop_rejected_finalizes_entry_consistently_with_event_path() {
        let (state, id, tv_raw) = bound_running_state("rej-1");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        registry.ensure_channel(id.clone());

        let outcome =
            TaskOutcome::rejected_for_tests(taskvisor::RejectionKind::QueueFull, "queue_full: 3/3");
        finalize_from_outcome(&state, &registry, tv_raw, &outcome);

        let task = state.get(&id).expect("rejected entry stays observable");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert!(
            registry.subscribe_raw(&id).is_none(),
            "output channel evicted"
        );
    }

    #[test]
    fn backstop_rejected_admission_drop_is_canceled() {
        let (state, id, tv_raw) = bound_running_state("rej-drop");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));

        let outcome = TaskOutcome::rejected_for_tests(
            taskvisor::RejectionKind::SlotBusy,
            "slot is busy; diagnostic text is not classification",
        );
        finalize_from_outcome(&state, &registry, tv_raw, &outcome);

        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Canceled,
            "a DropIfRunning skip is the task's own admission policy, not an error"
        );
    }

    #[test]
    fn backstop_stale_waiter_cannot_finalize_a_new_incarnation() {
        let (state, id, old_tv_raw) = bound_running_state("stale-waiter");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), Some(1));

        // The old managed task completes cleanup before the name can be reused.
        state.unbind(&id);

        // A resubmit wins the released name, but the new incarnation's bind_tv
        // has not happened yet.
        let spec = TaskSpec::builder("stale-waiter", TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("valid spec");
        state
            .reserve(id.clone(), spec)
            .expect("terminal name is reusable");

        finalize_from_outcome(&state, &registry, old_tv_raw, &TaskOutcome::Completed);

        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Pending,
            "a stale waiter must never finalize the new incarnation"
        );
    }

    #[test]
    fn backstop_rejected_user_drop_is_canceled() {
        let (state, id, tv_raw) = bound_running_state("rej-2");
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));

        let outcome = TaskOutcome::rejected_for_tests(
            taskvisor::RejectionKind::RemovedFromQueue,
            "removed_from_queue",
        );
        finalize_from_outcome(&state, &registry, tv_raw, &outcome);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Canceled);
    }

    #[test]
    fn backstop_noop_for_unknown_run_id() {
        let state = TaskState::new();
        let registry = Arc::new(OutputHub::new(OutputConfig::default()));
        finalize_from_outcome(&state, &registry, 999, &TaskOutcome::Completed);
    }
}
