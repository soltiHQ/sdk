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

use solti_runner::RunnerRouter;

use crate::system::init_uptime;
use crate::{
    error::CoreError,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    state::{StateConfig, StateSubscriber, TaskState, state_gc},
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
/// - [`StateConfig`] passed to [`enable_gc`](Self::enable_gc).
/// - [`solti_runner::RunnerRouter`] picks a runner for each submitted spec.
pub struct SupervisorApi {
    handle: SupervisorHandle,
    router: RunnerRouter,
    state: TaskState,
}

impl SupervisorApi {
    /// Create a supervisor with explicit configs and start its run loop in the background.
    /// - `sup_cfg`     - supervisor configuration;
    /// - `ctrl_cfg`    - controller configuration;
    /// - `subscribers` - event subscribers to attach to the supervisor;
    /// - `router`      - runner router [`solti_model::TaskKind`].
    ///
    /// The supervisor event loop is started via [`Supervisor::serve()`] which returns
    /// a [`SupervisorHandle`] for dynamic task management.
    pub fn new(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
    ) -> Result<Self, CoreError> {
        let state = TaskState::new();
        subscribers.push(Arc::new(StateSubscriber::new(state.clone())));

        let sup = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();

        let handle = sup.serve();
        init_uptime();

        info!("supervisor is ready to accept tasks");
        Ok(Self {
            handle,
            router,
            state,
        })
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

    /// Delete a task and its run history.
    ///
    /// Returns `true` if the task existed and was deleted.
    pub fn delete_task(&self, id: &TaskId) -> bool {
        self.state.delete_task(id)
    }

    /// Enable automatic garbage collection for in-memory state.
    ///
    /// Submits an embedded periodic task that sweeps expired runs and
    /// terminal tasks according to the provided [`StateConfig`].
    ///
    /// This is opt-in: if not called, no GC runs and the state grows unboundedly.
    ///
    /// # Example
    ///
    /// ```text
    /// let api = SupervisorApi::new(sup, ctrl, subs, router)?;
    /// api.enable_gc(StateConfig::default()).await?;
    /// ```
    pub async fn enable_gc(&self, config: StateConfig) -> Result<TaskId, CoreError> {
        let (task, spec) = state_gc(self.state.clone(), config);
        self.submit_with_task(task, &spec).await
    }

    /// Get a clone of the underlying supervisor handle.
    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
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

        self.state.add_task(task_id.clone(), spec.clone());

        let task_spec = TvTaskSpec::new(
            task,
            to_restart_policy(spec.restart())?,
            to_backoff_policy(spec.backoff())?,
            Some(Duration::from_millis(spec.timeout().as_millis())),
        );
        let controller_spec =
            ControllerSpec::new(to_admission_policy(spec.admission())?, task_spec);

        debug!("submitting pre-built task via controller");
        if let Err(e) = self.handle.submit(controller_spec).await {
            self.state.unregister_task(&task_id);
            return Err(CoreError::Supervisor(e.to_string()));
        }
        Ok(task_id)
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

    /// Cancel a running task by ID.
    ///
    /// This sends cancellation signal to the task and waits for confirmation with the configured grace period (from SupervisorConfig).
    ///
    /// The task must be cooperative and respect the `CancellationToken` passed during execution.
    ///
    /// Returns:
    /// - `Ok(())` if task was found and successfully cancelled
    /// - `Err(CoreError::Supervisor)` if task not found or cancellation timed out
    ///
    /// # Example
    /// ```text
    /// api.cancel_task(&task_id).await?;
    /// ```
    #[instrument(level = "debug", skip(self), fields(task_id = %id))]
    pub async fn cancel_task(&self, id: &TaskId) -> Result<(), CoreError> {
        debug!("cancelling task: {}", id);

        let was_cancelled = self
            .handle
            .cancel(id.as_str())
            .await
            .map_err(|e| CoreError::Supervisor(format!("cancel failed: {}", e)))?;

        if !was_cancelled {
            return Err(CoreError::Supervisor(format!("task not found: {}", id)));
        }

        debug!("task cancelled successfully: {}", id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use solti_model::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind};
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
    async fn submit_with_task_succeeds_for_simple_task() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
        )
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
    async fn submit_rejects_taskkind_embedded() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
        )
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
}
