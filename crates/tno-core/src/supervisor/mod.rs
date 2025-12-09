//! High-level API over taskvisor `Supervisor` used by tno-core.
//!
//! Responsibilities:
//! - owns a [`Supervisor`] instance and runs its event loop in the background;
//! - uses [`RunnerRouter`] to build concrete tasks from [`CreateSpec`];
//! - maps model-level specs / policies into controller specs and submits them.
use std::{sync::Arc, time::Duration};

use taskvisor::{
    ControllerConfig, ControllerSpec, Subscribe, Supervisor, SupervisorConfig, TaskRef, TaskSpec,
};
use tno_model::{CreateSpec, TaskId, TaskInfo, TaskStatus};
use tracing::{debug, info, instrument};

use crate::{
    error::CoreError,
    map::{to_admission_policy, to_backoff_policy, to_restart_policy},
    policy::TaskPolicy,
    router::RunnerRouter,
    state::{StateSubscriber, TaskState},
};

/// Thin wrapper around taskvisor [`Supervisor`] with a runner router.
///
/// This type is responsible for:
/// - constructing and running the supervisor;
/// - selecting a concrete runner for each [`CreateSpec`];
/// - mapping model-level specs into controller specs and submitting them.
pub struct SupervisorApi {
    sup: Arc<Supervisor>,
    router: RunnerRouter,
    state: TaskState,
}

impl SupervisorApi {
    /// Create a supervisor with explicit configs and start its run loop in the background.
    /// - `sup_cfg`     — supervisor configuration;
    /// - `ctrl_cfg`    — controller configuration;
    /// - `subscribers` — event subscribers to attach to the supervisor;
    /// - `router`      — runner router [`tno_model::TaskKind`].
    ///
    /// The supervisor run loop is spawned on the current Tokio runtime.
    /// This method waits until the supervisor reports readiness before returning.
    pub async fn new(
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

        let runner = Arc::clone(&sup);
        tokio::spawn(async move {
            if let Err(e) = runner.run(Vec::new()).await {
                panic!("supervisor run loop exited with error: {}", e)
            }
        });

        sup.wait_ready().await;
        info!("supervisor is ready to accept tasks");
        Ok(Self { sup, router, state })
    }

    /// Get task information by ID.
    pub fn get_task(&self, id: &TaskId) -> Option<TaskInfo> {
        self.state.get(id)
    }

    /// List all tasks in a specific slot.
    pub fn list_tasks_by_slot(&self, slot: &str) -> Vec<TaskInfo> {
        self.state.list_by_slot(slot)
    }

    /// List all tasks.
    pub fn list_all_tasks(&self) -> Vec<TaskInfo> {
        self.state.list_all()
    }

    /// List tasks by status.
    pub fn list_tasks_by_status(&self, status: TaskStatus) -> Vec<TaskInfo> {
        self.state.list_by_status(status)
    }

    /// Get a clone of the underlying supervisor handle.
    pub fn supervisor(&self) -> Arc<Supervisor> {
        Arc::clone(&self.sup)
    }

    /// Build and submit a task described by [`CreateSpec`].
    ///
    /// Steps:
    /// 1. Ask the [`RunnerRouter`] to pick a runner and build a [`TaskRef`].
    /// 2. Convert [`CreateSpec`] into [`TaskPolicy`] (dropping the [`tno_model::TaskKind`] information).
    /// 3. Delegate to [`SupervisorApi::submit_with_task`].
    ///
    /// This is the primary entrypoint for tasks that are fully described by the public [`tno_model::TaskKind`] model.
    #[instrument(level = "debug", skip(self, spec), fields(slot = %spec.slot, kind = ?spec.kind))]
    pub async fn submit(&self, spec: &CreateSpec) -> Result<TaskId, CoreError> {
        let task = self.router.build(spec)?;
        let task_id = TaskId::from(task.name());

        self.state.add_task(task_id.clone(), spec.slot.clone());
        let policy = TaskPolicy::from_spec(spec);

        self.submit_with_task(task, &policy).await?;
        Ok(task_id)
    }

    /// Submit a pre-built task together with its runtime policy.
    ///
    /// This API is intended for in-process / code-defined tasks (without `TaskKind`).
    ///
    /// The caller is responsible for constructing the [`TaskRef`];
    /// `TaskPolicy` controls slot, timeout, restart and backoff behavior.
    #[instrument(level = "debug", skip(self, task, policy), fields(slot = %policy.slot))]
    pub async fn submit_with_task(
        &self,
        task: TaskRef,
        policy: &TaskPolicy,
    ) -> Result<TaskId, CoreError> {
        let task_id = TaskId::from(task.name());
        self.state.add_task(task_id.clone(), policy.slot.clone());

        let task_spec = TaskSpec::new(
            task,
            to_restart_policy(policy.restart),
            to_backoff_policy(&policy.backoff),
            Some(Duration::from_millis(policy.timeout_ms)),
        );
        let controller_spec = ControllerSpec {
            admission: to_admission_policy(policy.admission),
            task_spec,
        };

        debug!("submitting pre-built task via controller");
        self.sup
            .submit(controller_spec)
            .await
            .map_err(|e| CoreError::Supervisor(e.to_string()))?;
        Ok(task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use taskvisor::{TaskError, TaskFn};
    use tno_model::{
        AdmissionStrategy, BackoffStrategy, JitterStrategy, Labels, RestartStrategy, TaskKind,
    };
    use tokio_util::sync::CancellationToken;

    fn mk_backoff() -> BackoffStrategy {
        BackoffStrategy {
            jitter: JitterStrategy::Equal,
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
        .await
        .expect("failed to create SupervisorApi");

        // Простейшая задача, которая сразу успешно завершается.
        let task: TaskRef = TaskFn::arc("test-task", |_ctx: CancellationToken| async move {
            Ok::<(), TaskError>(())
        });

        let policy = TaskPolicy::new(
            "test-slot".to_string(),
            1_000,
            RestartStrategy::Never,
            mk_backoff(),
            AdmissionStrategy::DropIfRunning,
        );

        let res = api.submit_with_task(task, &policy).await;
        match res {
            Ok(task_id) => {
                assert!(!task_id.as_str().is_empty());
                assert!(task_id.as_str().contains("test-task"));
            }
            Err(e) => panic!("expected Ok(TaskId), got error: {e:?}"),
        }
    }

    #[tokio::test]
    async fn submit_rejects_taskkind_none() {
        let router = RunnerRouter::new();
        let api = SupervisorApi::new(
            SupervisorConfig::default(),
            ControllerConfig::default(),
            Vec::new(),
            router,
        )
        .await
        .expect("failed to create SupervisorApi");

        let spec = CreateSpec {
            slot: "test-slot-none".to_string(),
            kind: TaskKind::None,
            timeout_ms: 1_000,
            restart: RestartStrategy::Never,
            backoff: mk_backoff(),
            admission: AdmissionStrategy::DropIfRunning,
            labels: Labels::default(),
        };
        let res = api.submit(&spec).await;

        match res {
            Err(CoreError::NoRunner(msg)) => {
                assert!(msg.contains("TaskKind::None"));
            }
            Ok(_) => panic!("expected error for TaskKind::None, got Ok(TaskId)"),
            Err(e) => panic!("expected CoreError::NoRunner, got {e:?}"),
        }
    }
}
