//! High-level API over taskvisor `Supervisor` used by solti-core.
//!
//! Responsibilities:
//! - owns a [`Supervisor`] instance and runs its event loop in the background;
//! - uses [`RunnerRouter`] to build concrete tasks from [`TaskSpec`];
//! - maps model-level specs / policies into controller specs and submits them.
use std::{sync::Arc, time::Duration};

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
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
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
    /// Supervisor grace period: cancellation confirmation must wait slightly
    /// longer, because the registry force-aborts stragglers only *after* it.
    grace: Duration,
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
    pub async fn new_with_output_registry(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
        state_cfg: StateConfig,
        output_registry: Arc<OutputRegistry>,
    ) -> Result<Self, CoreError> {
        let state = TaskState::new();
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
    /// - Running: cooperative cancel confirmed by `TaskRemoved`. The registry
    ///   force-aborts stragglers only *after* the grace period, so the wait is
    ///   `grace + 1s` (a bare `grace` deterministically times out on stuck tasks).
    /// - Queued (not in the registry): `remove(id)` lets the controller purge the
    ///   spec from its slot queue; the entry is finalized as `Canceled` by the
    ///   `ControllerRejected("removed_from_queue")` event.
    async fn cancel_bound(&self, id: &TaskId) -> Result<bool, CoreError> {
        let Some(tv) = self.state.tv_for(id) else {
            // No binding (already finished, or pre-0.3 path): label fallback.
            return self
                .handle
                .cancel_by_label(id.as_str())
                .await
                .map_err(|e| CoreError::Supervisor(format!("cancel failed: {}", e)));
        };
        let tv = taskvisor::TaskId::from_raw(tv);

        let cancelled = self
            .handle
            .cancel_with_timeout(tv, self.grace + Duration::from_secs(1))
            .await
            .map_err(|e| CoreError::Supervisor(format!("cancel failed: {}", e)))?;
        if cancelled {
            return Ok(true);
        }

        // Not in the registry: possibly still queued in the controller.
        self.handle
            .remove(tv)
            .map_err(|e| CoreError::Supervisor(format!("remove failed: {}", e)))?;
        Ok(false)
    }

    /// Get a clone of the underlying supervisor handle.
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }

    /// Get a clone of the shared [`TaskState`].
    ///
    /// The clone is cheap (`Arc<RwLock<_>>` inside) and reflects live state:
    /// later mutations on the original are visible through the clone.
    ///
    /// Intended for read-only consumers like metric collectors
    /// (e.g. `solti_prometheus::PrometheusStateCollector`).
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
    #[instrument(level = "debug", skip(self, task, spec), fields(slot = %spec.slot()))]
    pub async fn submit_with_task(
        &self,
        task: TaskRef,
        spec: &TaskSpec,
    ) -> Result<TaskId, CoreError> {
        let task_id = TaskId::from(task.name());

        // A live (non-terminal) entry under this name means the previous
        // incarnation is still active: registering provisionally would clobber
        // its state and output channel before any admission decision is made.
        if let Some(existing) = self.state.get(&task_id)
            && !existing.status().phase.is_terminal()
        {
            return Err(CoreError::Supervisor(format!(
                "task '{task_id}' is already active (phase {})",
                existing.status().phase
            )));
        }

        // Build the controller spec *before* touching state, so mapping errors
        // do not leave a provisional entry behind.
        let task_spec = TvTaskSpec::new(
            task,
            to_restart_policy(spec.restart())?,
            to_backoff_policy(spec.backoff())?,
            Some(Duration::from_millis(spec.timeout().as_millis())),
        )
        .with_max_retries(spec.max_retries())
        .with_slot(spec.slot().as_str());
        let controller_spec =
            ControllerSpec::new(to_admission_policy(spec.admission())?, task_spec);

        self.state.add_task(task_id.clone(), spec.clone());

        debug!("submitting pre-built task via controller");
        match self.handle.submit_and_watch(controller_spec).await {
            Ok((tv_id, waiter)) => {
                // Bind the entry to its run identity: from here on, lossy events
                // (including async rejections) resolve through this binding.
                self.state.bind_tv(&task_id, tv_id.get());

                // Guaranteed-outcome backstop: taskvisor delivers the *final*
                // `TaskOutcome` on a oneshot that survives bus lag. We finalize the
                // state entry from it, so a dropped terminal event can never leave a
                // task wedged in a non-terminal phase. This complements the
                // event-driven `StateSubscriber` (which owns per-attempt/output);
                // it acts only when events have NOT already finalized the entry.
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
                Err(CoreError::Supervisor(e.to_string()))
            }
        }
    }

    /// Backstop finalization of a state entry from taskvisor's guaranteed [`TaskOutcome`].
    ///
    /// Idempotent and event-friendly: resolves the run identity, and acts **only** if the
    /// entry exists and is not already in a terminal phase — so when the lossy event path
    /// finalized the entry first, this is a no-op (no spurious illegal-transition warning).
    /// A `Rejected` outcome (the submission never ran) drops the provisional entry.
    fn finalize_from_outcome(
        state: &TaskState,
        output_registry: &OutputRegistry,
        tv_raw: u64,
        outcome: &taskvisor::TaskOutcome,
    ) {
        use taskvisor::TaskOutcome;

        let Some(model_id) = state.resolve_tv(tv_raw) else {
            return; // events already cleaned the entry up, or it was never bound
        };

        if matches!(outcome, TaskOutcome::Rejected { .. }) {
            // Never admitted: drop the provisional entry (idempotent with the event path).
            state.unregister_task(&model_id);
            output_registry.evict(&model_id);
            return;
        }

        // Leave an already-finalized entry alone.
        if state
            .get(&model_id)
            .is_none_or(|t| t.status().phase.is_terminal())
        {
            return;
        }

        let (phase, error, exit_code) = match outcome {
            TaskOutcome::Completed => (TaskPhase::Succeeded, None, None),
            TaskOutcome::Failed { reason, exit_code } => {
                (TaskPhase::Exhausted, Some(reason.to_string()), *exit_code)
            }
            TaskOutcome::Fatal { reason, exit_code } => {
                (TaskPhase::Failed, Some(reason.to_string()), *exit_code)
            }
            TaskOutcome::Canceled => (TaskPhase::Canceled, None, None),
            TaskOutcome::ForceAborted => (
                TaskPhase::Canceled,
                Some("force_terminated_after_grace".to_string()),
                None,
            ),
            TaskOutcome::Panicked => (TaskPhase::Failed, Some("actor panicked".to_string()), None),
            // `Rejected` handled above; `#[non_exhaustive]` future variants are
            // recorded conservatively as a failure rather than silently ignored.
            _ => (
                TaskPhase::Failed,
                Some("unknown task outcome".to_string()),
                None,
            ),
        };
        state.transition_finished(&model_id, phase, error, exit_code);
    }

    /// Roll back resources reserved by [`submit_with_task`] before `handle.submit`.
    fn unwind_provisional_submit(&self, task_id: &TaskId) {
        self.state.unregister_task(task_id);
        self.output_registry.evict(task_id);
    }

    /// Gracefully shut down the supervisor: cancel all tasks and wait for completion.
    ///
    /// Consumes `self` - no further operations are possible after shutdown.
    /// The grace period is determined by [`SupervisorConfig`] passed to [`new`](Self::new).
    ///
    /// # Example
    /// ```text
    /// api.shutdown().await?;
    /// ```
    #[instrument(level = "info", skip(self))]
    pub async fn shutdown(self) -> Result<(), CoreError> {
        info!("initiating graceful shutdown");
        self.handle
            .shutdown()
            .await
            .map_err(|e| CoreError::Supervisor(e.to_string()))
    }

    /// Cancel a task by ID (in-process Rust API), running or still queued.
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn cancel_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("cancelling task: {}", id);

        let was_running = self.cancel_bound(id).await?;
        if !was_running && self.state.get(id).is_none() {
            return Err(CoreError::Supervisor(format!("task not found: {}", id)));
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
    use taskvisor::{TaskError, TaskFn};
    use tokio_util::sync::CancellationToken;

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

        let task: TaskRef = TaskFn::arc("budget-task", |_ctx: CancellationToken| async move {
            Err::<(), TaskError>(TaskError::Fail {
                reason: "always fails".into(),
                exit_code: None,
            })
        });

        let spec = TaskSpec::builder("budget-slot", TaskKind::Embedded, 5_000_u64)
            .restart(RestartPolicy::OnFailure)
            .backoff(solti_model::BackoffPolicy {
                jitter: solti_model::JitterPolicy::None,
                first_ms: 1,
                max_ms: 1,
                factor: 1.0,
            })
            .max_retries(2)
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");

        let task_id = api.submit_with_task(task, &spec).await.expect("submit ok");

        // Budget = 2 retries -> exactly 3 attempts (runs survive unregister).
        // Without the mapping the task retries forever and runs keep growing.
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

        let task: TaskRef = TaskFn::arc("test-task", |_ctx: CancellationToken| async move {
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
        let task: TaskRef = TaskFn::arc("kill-me", move |ctx: CancellationToken| {
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

    // --- guaranteed-outcome backstop (finalize_from_outcome) ---

    use taskvisor::TaskOutcome;

    fn bound_running_state(name: &str, tv: u64) -> (TaskState, TaskId) {
        let state = TaskState::new();
        let id = TaskId::from(name);
        let spec = TaskSpec::builder(name, TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("valid spec");
        state.add_task(id.clone(), spec);
        state.bind_tv(&id, tv);
        state.transition_starting(&id); // -> Running (non-terminal)
        (state, id)
    }

    #[test]
    fn backstop_finalizes_a_lost_terminal_outcome() {
        // Models a dropped ActorExhausted/TaskRemoved: the entry is still Running,
        // but the guaranteed outcome must finalize it instead of leaving a zombie.
        let (state, id) = bound_running_state("lost-1", 101);
        let registry = OutputRegistry::default();

        SupervisorApi::finalize_from_outcome(&state, &registry, 101, &TaskOutcome::Completed);

        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
    }

    #[test]
    fn backstop_is_noop_when_events_already_finalized() {
        // Common case: events finalized first; the backstop must not fight them.
        let (state, id) = bound_running_state("done-1", 102);
        let registry = OutputRegistry::default();
        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), Some(1));

        SupervisorApi::finalize_from_outcome(&state, &registry, 102, &TaskOutcome::Completed);

        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Failed,
            "an already-terminal entry must be left untouched"
        );
    }

    #[test]
    fn backstop_rejected_drops_the_provisional_entry() {
        let (state, id) = bound_running_state("rej-1", 103);
        let registry = OutputRegistry::default();

        SupervisorApi::finalize_from_outcome(
            &state,
            &registry,
            103,
            &TaskOutcome::Rejected {
                reason: "dropped: slot busy (running)".into(),
            },
        );

        assert!(
            state.get(&id).is_none(),
            "a rejected submission never ran; its provisional entry must be removed"
        );
    }

    #[test]
    fn backstop_noop_for_unknown_run_id() {
        let state = TaskState::new();
        let registry = OutputRegistry::default();
        // No panic, no effect: nothing is bound to tv=999.
        SupervisorApi::finalize_from_outcome(&state, &registry, 999, &TaskOutcome::Completed);
    }
}
