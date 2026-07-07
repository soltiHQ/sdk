//! # Supervisor API.
//!
//! High-level API over taskvisor `Supervisor` used by solti-core.
//!
//! Responsibilities:
//! - owns a [`Supervisor`] instance and runs its event loop in the background;
//! - uses [`RunnerRouter`] to build concrete tasks from [`TaskSpec`];
//! - maps model-level specs / policies into controller specs and submits them.
//!
//! ## Two-plane state reconstruction (invariant)
//!
//! Task state is rebuilt from two planes with **disjoint authority**:
//!
//! - The **event plane** (`StateSubscriber`, fed by taskvisor's *lossy* broadcast bus) is authoritative for per-attempt detail: phase transitions, `TaskRun` records, and output announcements.
//! - The **completion plane** (`finalize_from_outcome`, fed by the *guaranteed* per-submission `TaskWaiter` oneshot) is authoritative **only** for terminal
//!   liveness - it ensures a run still reaches a terminal phase when its terminal event was dropped under bus lag.
//!
//! The backstop must therefore stay a **no-op on already-terminal entries** (enforced here and by `Task::transition_finished`'s sticky-terminal guard).
//! It can never demote a richer event-derived phase.
//! Both planes classify outcomes and rejections through one crosswalk (`map::phase`). They can never disagree on the final phase.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use solti_model::{Task, TaskId, TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec};
use taskvisor::{
    ControllerConfig, ControllerSpec, Subscribe, Supervisor, SupervisorConfig, SupervisorHandle,
    TaskRef, TaskSpec as TvTaskSpec,
};
use tracing::{debug, info, instrument};

use solti_runner::{OutputRegistry, RunnerRouter};

use crate::system::init_uptime;
use crate::{
    error::CoreError,
    map::{phase, to_admission_policy, to_backoff_policy, to_restart_policy},
    state::{StateConfig, StateSubscriber, TaskState, state_sweep},
};

/// Thin wrapper around taskvisor [`Supervisor`] with a runner router.
///
/// This type is responsible for:
/// - constructing and running the supervisor;
/// - selecting a concrete runner for each [`TaskSpec`];
/// - mapping model-level specs into controller specs and submitting them.
///
/// ## Also
///
/// - [`CoreError`] error type returned by all methods.
/// - [`StateConfig`] configures sweep TTLs and interval (defaults are sane).
/// - [`solti_runner::RunnerRouter`] picks a runner for each submitted spec.
pub struct SupervisorApi {
    output_registry: Arc<OutputRegistry>,
    handle: SupervisorHandle,
    router: RunnerRouter,
    state: TaskState,
    grace: Duration,
    shutdown_started: AtomicBool,
}

impl Drop for SupervisorApi {
    fn drop(&mut self) {
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let handle = self.handle.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    let _ = handle.shutdown().await;
                });
            }
            Err(_) => {
                tracing::warn!(
                    "SupervisorApi dropped without shutdown() and outside a Tokio runtime; \
                     the taskvisor runtime may leak — call SupervisorApi::shutdown().await",
                );
            }
        }
    }
}

impl SupervisorApi {
    /// Create a supervisor with explicit configs and start its run loop in the background.
    ///
    /// - `sup_cfg`      - supervisor configuration;
    /// - `ctrl_cfg`     - controller configuration;
    /// - `subscribers`  - event subscribers to attach to the supervisor;
    /// - `router`       - runner router [`solti_model::TaskKind`];
    /// - `state_cfg`    - sweep TTLs and interval ([`StateConfig::default()`] is usually fine).
    ///
    /// The supervisor event loop is started via [`Supervisor::serve()`] which returns a [`SupervisorHandle`] for dynamic task management.
    ///
    /// A periodic sweep task is automatically submitted to prevent unbounded memory growth.
    /// It removes completed runs and terminal tasks that exceed their configured TTLs.
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the runtime refused the embedded sweep task submission.
    ///   The sweep spec itself is built internally and always valid; no other variant is reachable.
    pub async fn new(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
    ) -> Result<Self, CoreError> {
        Self::new_with_output_registry(
            sup_cfg,
            ctrl_cfg,
            subscribers,
            router,
            state_cfg,
            Arc::new(OutputRegistry::default()),
        )
        .await
    }

    /// Same as [`SupervisorApi::new`], but lets the caller pass a shared [`OutputRegistry`].
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the runtime refused the embedded sweep task submission.
    ///   The sweep spec itself is built internally and always valid; no other variant is reachable.
    pub async fn new_with_output_registry(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
        output_registry: Arc<OutputRegistry>,
    ) -> Result<Self, CoreError> {
        let state = TaskState::new();
        state.set_max_runs_per_task(state_cfg.max_runs_per_task);
        subscribers.push(Arc::new(StateSubscriber::with_output_registry(
            state.clone(),
            Arc::clone(&output_registry),
        )));

        let grace = sup_cfg.grace;
        let sup = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();

        let handle = sup.serve();
        init_uptime();

        let api = Self {
            handle,
            router,
            state,
            output_registry,
            grace,
            shutdown_started: AtomicBool::new(false),
        };

        let (task, spec) = state_sweep(api.state.clone(), state_cfg);
        api.submit_with_task(task, &spec).await?;
        info!("supervisor is ready (sweep active)");

        Ok(api)
    }

    /// Get a shared handle to the output registry for live-tail subscriptions.
    pub fn output_registry(&self) -> &Arc<OutputRegistry> {
        &self.output_registry
    }

    /// Get task information by ID.
    pub fn get_task(&self, id: &TaskId) -> Option<Task> {
        self.state.get(id)
    }

    /// List all tasks in a specific slot.
    pub fn list_tasks_by_slot(&self, slot: &str) -> Vec<Task> {
        self.state.list_by_slot(slot)
    }

    /// List all tasks.
    pub fn list_all_tasks(&self) -> Vec<Task> {
        self.state.list_all()
    }

    /// List tasks by phase.
    pub fn list_tasks_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        self.state.list_by_status(phase)
    }

    /// Query tasks with combined filters and pagination.
    pub fn query_tasks(&self, query: &TaskQuery) -> TaskPage<Task> {
        self.state.query(query)
    }

    /// List execution history for a specific task (oldest first).
    pub fn list_task_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        self.state.list_runs(id)
    }

    /// Stop a task (running **or** still queued in its slot) and purge its run history.
    ///
    /// Deleting a missing task is not an error: the call is an idempotent no-op.
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "cancel"`): the runtime failed to cancel the bound run.
    /// - [`CoreError::Supervisor`] (`op = "remove"`): the run did not confirm the cancel in time and the follow-up queue purge failed.
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn delete_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("deleting task: {}", id);

        let was_cancelled = self.cancel_bound(id).await?;
        let had_local = self.state.delete_task(id);

        if !was_cancelled && !had_local {
            debug!("delete_task: no such task in supervisor or state; idempotent no-op");
        }
        Ok(())
    }

    /// Cancel the taskvisor run bound to `id`, covering both running and queued tasks.
    ///
    /// - Running: cooperative cancel confirmed by `TaskRemoved`.
    /// - Queued (not in the registry): `remove(id)` lets the controller purge the spec from its slot queue.
    async fn cancel_bound(&self, id: &TaskId) -> Result<bool, CoreError> {
        let Some(tv) = self.state.tv_for(id) else {
            return self
                .handle
                .cancel_by_label(id.as_str())
                .await
                .map_err(|e| CoreError::supervisor("cancel", e));
        };

        let cancelled = self
            .handle
            .cancel_with_timeout(tv, self.grace + Duration::from_secs(1))
            .await
            .map_err(|e| CoreError::supervisor("cancel", e))?;
        if cancelled {
            return Ok(true);
        }

        self.handle
            .remove(tv)
            .map_err(|e| CoreError::supervisor("remove", e))?;
        Ok(false)
    }

    /// Get a clone of the underlying supervisor handle.
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Get a clone of the shared [`TaskState`].
    ///
    /// The clone is cheap (`Arc<RwLock<_>>` inside) and reflects live state: later mutations on the original are visible through the clone.
    /// Intended for read-only consumers like metric collectors (e.g. `solti_prometheus::PrometheusStateCollector`).
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
    ///
    /// ## Errors
    ///
    /// - [`CoreError::InvalidSpec`]: the spec failed validation. This covers structural problems and `TaskKind::Embedded`, which cannot go through runners (use [`SupervisorApi::submit_with_task`]).
    /// - [`CoreError::Runner`]: no registered runner matches the spec's kind/selector, or the runner failed to build the task.
    /// - [`CoreError::Mapping`]: a spec policy has no taskvisor equivalent (unknown `#[non_exhaustive]` variant), or the backoff parameters are invalid.
    /// - [`CoreError::AlreadyExists`]: a non-terminal task with the same name is still active.
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the runtime rejected the submission.
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
    /// ## Errors
    ///
    /// - [`CoreError::InvalidSpec`]: the task name fails `TaskId` format validation.
    /// - [`CoreError::Mapping`]: a spec policy has no taskvisor equivalent (unknown `#[non_exhaustive]` variant), or the backoff parameters are invalid.
    /// - [`CoreError::AlreadyExists`]: a non-terminal task with the same name is still active.
    /// - [`CoreError::Supervisor`] (`op = "submit"`): the runtime rejected the submission; the provisional state entry is rolled back.
    #[instrument(level = "debug", skip(self, task, spec), fields(slot = %spec.slot()))]
    pub async fn submit_with_task(
        &self,
        task: TaskRef,
        spec: &TaskSpec,
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

        self.state.reserve(task_id.clone(), spec.clone())?;

        debug!("submitting pre-built task via controller");
        match self.handle.submit_and_watch(controller_spec).await {
            Ok((tv_id, waiter)) => {
                self.state.bind_tv(&task_id, tv_id);

                let state = self.state.clone();
                let output_registry = Arc::clone(&self.output_registry);
                let tv_raw = tv_id.get();
                tokio::spawn(async move {
                    if let Ok(outcome) = waiter.wait().await {
                        Self::finalize_from_outcome(&state, &output_registry, tv_raw, &outcome);
                    }
                });

                Ok(task_id)
            }
            Err(e) => {
                self.unwind_provisional_submit(&task_id);
                Err(CoreError::supervisor("submit", e))
            }
        }
    }

    /// Backstop finalization of a state entry from taskvisor's guaranteed [`TaskOutcome`].
    ///
    /// Idempotent and event-friendly: resolves the run identity, and acts **only** if the entry exists and is not already in a terminal phase.
    /// A `Rejected` outcome (the submission never ran) drops the provisional entry.
    fn finalize_from_outcome(
        state: &TaskState,
        output_registry: &OutputRegistry,
        tv_raw: u64,
        outcome: &taskvisor::TaskOutcome,
    ) {
        use taskvisor::TaskOutcome;

        let Some(model_id) = state.resolve_tv(tv_raw) else {
            return;
        };

        // A rejection finalizes unconditionally: the submission never ran, the
        // provisional entry must reach a terminal phase even if the event plane
        // already touched it (transition_finished stays sticky-terminal).
        if !matches!(outcome, TaskOutcome::Rejected { .. })
            && state
                .get(&model_id)
                .is_none_or(|t| t.status().phase.is_terminal())
        {
            return;
        }

        let (phase, error, exit_code) = phase::phase_for_outcome(outcome);
        state.transition_finished(&model_id, phase, error, exit_code);
        output_registry.evict(&model_id);
        state.unbind(&model_id);
    }

    /// Roll back resources reserved by [`submit_with_task`] before `handle.submit`.
    fn unwind_provisional_submit(&self, task_id: &TaskId) {
        self.state.unregister_task(task_id);
        self.output_registry.evict(task_id);
    }

    /// Gracefully shut down the supervisor: cancel all tasks and wait for completion.
    ///
    /// ## Errors
    ///
    /// - [`CoreError::Supervisor`] (`op = "shutdown"`): the runtime failed to stop cleanly (for example, tasks had to be force-aborted after the grace period).
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(self) -> Result<(), CoreError> {
        info!("initiating graceful shutdown");
        let res = self
            .handle
            .clone()
            .shutdown()
            .await
            .map_err(|e| CoreError::supervisor("shutdown", e));
        self.shutdown_started.store(true, Ordering::Release);
        res
    }

    /// Cancel a task by ID (in-process Rust API), running or still queued.
    ///
    /// ## Errors
    ///
    /// - [`CoreError::NotFound`]: nothing was cancelled and no state entry exists for `id`.
    /// - [`CoreError::Supervisor`] (`op = "cancel"`): the runtime failed to cancel the bound run.
    /// - [`CoreError::Supervisor`] (`op = "remove"`): the run did not confirm the cancel in time and the follow-up queue purge failed.
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn cancel_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("cancelling task: {}", id);

        let was_running = self.cancel_bound(id).await?;
        if !was_running && self.state.get(id).is_none() {
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
    use solti_runner::OutputRegistry;
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
        assert!(runs.iter().all(|r| r.phase == TaskPhase::Failed));
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

        let res = api.submit_with_task(task, &spec).await;
        match res {
            Ok(task_id) => {
                assert!(!task_id.as_str().is_empty());
                assert!(task_id.as_str().contains("test-task"));
            }
            Err(e) => panic!("expected Ok(TaskId), got error: {e:?}"),
        }
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

        for _ in 0..100 {
            if cancelled_observed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            cancelled_observed.load(Ordering::SeqCst),
            "task body must observe the cancel token — delete must cancel, not just wipe state"
        );
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
    async fn supervisor_api_default_new_creates_empty_output_registry() {
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

        assert_eq!(api.output_registry().active_channels(), 0);
    }

    #[tokio::test]
    async fn supervisor_api_with_provided_registry_shares_arc() {
        let router = RunnerRouter::new();
        let registry = Arc::new(OutputRegistry::new(64));
        registry.ensure_channel(TaskId::from("seeded"));

        let api = SupervisorApi::new_with_output_registry(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
            Arc::clone(&registry),
        )
        .await
        .expect("SupervisorApi::new_with_output_registry");

        assert!(Arc::ptr_eq(api.output_registry(), &registry));
        assert_eq!(api.output_registry().active_channels(), 1);
    }

    #[tokio::test]
    async fn unwind_provisional_submit_drops_state_entry_and_output_channel() {
        let registry = Arc::new(OutputRegistry::default());
        let router = RunnerRouter::new();
        let api = SupervisorApi::new_with_output_registry(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
            StateConfig::default(),
            Arc::clone(&registry),
        )
        .await
        .expect("SupervisorApi::new_with_output_registry");

        let ghost = TaskId::from("orphan-on-submit-fail");

        registry.ensure_channel(ghost.clone());
        let spec = TaskSpec::builder("ghost-slot", TaskKind::Embedded, 1_000_u64)
            .restart(RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");
        api.state.add_task(ghost.clone(), spec);

        let channels_before = registry.active_channels();
        assert!(channels_before >= 1, "channel must exist before unwind");
        assert!(api.get_task(&ghost).is_some(), "state entry must exist");

        api.unwind_provisional_submit(&ghost);
        assert_eq!(
            registry.active_channels(),
            channels_before - 1,
            "unwind must drop exactly the ghost task's channel"
        );
        assert!(
            api.get_task(&ghost).is_none(),
            "state entry must be gone after unwind"
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

    #[test]
    fn backstop_finalizes_a_lost_terminal_outcome() {
        let (state, id, tv_raw) = bound_running_state("lost-1");
        let registry = OutputRegistry::default();

        SupervisorApi::finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
    }

    #[test]
    fn backstop_evicts_output_channel_on_terminal_outcome() {
        let (state, id, tv_raw) = bound_running_state("leak-1");
        let registry = OutputRegistry::default();
        registry.ensure_channel(id.clone());
        assert!(registry.subscribe(&id).is_some(), "channel exists before");

        SupervisorApi::finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
        assert!(
            registry.subscribe(&id).is_none(),
            "the backstop must evict the output channel on a terminal outcome"
        );
    }

    #[test]
    fn backstop_is_noop_when_events_already_finalized() {
        let (state, id, tv_raw) = bound_running_state("done-1");
        let registry = OutputRegistry::default();
        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), Some(1));

        SupervisorApi::finalize_from_outcome(&state, &registry, tv_raw, &TaskOutcome::Completed);

        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Failed,
            "an already-terminal entry must be left untouched"
        );
    }

    #[test]
    fn backstop_rejected_finalizes_entry_consistently_with_event_path() {
        let (state, id, tv_raw) = bound_running_state("rej-1");
        let registry = OutputRegistry::default();
        registry.ensure_channel(id.clone());

        SupervisorApi::finalize_from_outcome(
            &state,
            &registry,
            tv_raw,
            &TaskOutcome::Rejected {
                reason: "dropped: slot busy (running)".into(),
            },
        );

        let task = state.get(&id).expect("rejected entry stays observable");
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert!(registry.subscribe(&id).is_none(), "output channel evicted");
    }

    #[test]
    fn backstop_rejected_user_drop_is_canceled() {
        let (state, id, tv_raw) = bound_running_state("rej-2");
        let registry = OutputRegistry::default();

        SupervisorApi::finalize_from_outcome(
            &state,
            &registry,
            tv_raw,
            &TaskOutcome::Rejected {
                reason: "removed_from_queue".into(),
            },
        );

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Canceled);
    }

    #[test]
    fn backstop_noop_for_unknown_run_id() {
        let state = TaskState::new();
        let registry = OutputRegistry::default();
        SupervisorApi::finalize_from_outcome(&state, &registry, 999, &TaskOutcome::Completed);
    }
}
