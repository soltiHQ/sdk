use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::{Condvar, Mutex};
use solti_chain::{ChainRunner, ChainSpec, ChainStep};
use solti_model::{
    AdmissionPolicy, ConditionStatus, EmbeddedSpec, Flag, LabelSelector, Labels, Slot,
    SubprocessMode, SubprocessSpec, TaskEnv, TaskPhase, TaskSpec, TaskWorkload,
    WORKLOAD_API_VERSION, WorkloadTypeMeta,
};
use solti_runner::{BuildContext, RunId, Runner, RunnerError};
use taskvisor::{
    BoxTaskFuture, SupervisorConfig, Task as TvTask, TaskContext, TaskError, TaskFn,
    TaskOutcomeKind, TaskSpec as TvTaskSpec,
};
use tokio_stream::StreamExt;
use tokio_util::task::TaskTracker;

use super::*;
use crate::{ReconciliationConfig, StateConfig};

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

fn routed_to(name: &str, timeout_ms: u64, backend: &str) -> TaskManifest {
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "true".into(),
            args: vec![],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let mut labels = Labels::new();
    labels.insert("backend", backend);
    TaskManifest::new(
        name,
        TaskSpec::builder("routed-slot", workload, timeout_ms)
            .runner_selector(LabelSelector::from_labels(labels))
            .build()
            .unwrap(),
    )
    .unwrap()
}

fn subprocess_workload_types() -> Vec<WorkloadTypeMeta> {
    vec![WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").expect("built-in workload GVK")]
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

fn immediate_task() -> TaskRef {
    TaskFn::arc(|_ctx: TaskContext| async move { Ok::<(), TaskError>(()) })
}

fn cancellable_task() -> TaskRef {
    TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Err::<(), TaskError>(TaskError::Canceled)
    })
}

async fn api(router: RunnerRouter) -> SupervisorApi {
    SupervisorApi::builder(router).start().await.unwrap()
}

async fn api_with_reconciliation(
    router: RunnerRouter,
    reconciliation: ReconciliationConfig,
) -> SupervisorApi {
    SupervisorApi::builder(router)
        .with_reconciliation_config(reconciliation)
        .start()
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

async fn wait_for_binding(api: &SupervisorApi, name: &TaskId, generation: u64) -> RuntimeBinding {
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

async fn wait_for_taskvisor_name(api: &SupervisorApi, tv: taskvisor::TaskId) -> Arc<str> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some((_, name)) = api
                .reconciler
                .handle
                .list()
                .await
                .into_iter()
                .find(|(id, _)| *id == tv)
            {
                return name;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Taskvisor registration did not appear")
}

struct RecordingRunner {
    seen: Arc<Mutex<Vec<(TaskId, u64, String)>>>,
}

struct IdentityRunner {
    allocated_name: Arc<Mutex<Option<String>>>,
}

#[solti_runner::async_trait]
impl Runner for RecordingRunner {
    fn name(&self) -> &str {
        "recording"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        self.seen.lock().push((
            task.name().clone(),
            task.metadata().generation(),
            task.metadata().resource_version().to_string(),
        ));
        Ok(immediate_task())
    }
}

#[solti_runner::async_trait]
impl Runner for IdentityRunner {
    fn name(&self) -> &str {
        "identity"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        self.allocated_name.lock().replace(run_id.name().to_owned());
        Ok(TaskFn::arc(|ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err::<(), TaskError>(TaskError::Canceled)
        }))
    }
}

#[tokio::test]
async fn taskvisor_spec_identity_uses_routed_and_core_allocated_run_names() {
    let allocated_name = Arc::new(Mutex::new(None));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(IdentityRunner {
            allocated_name: Arc::clone(&allocated_name),
        }))
        .unwrap();
    let api = api(router).await;

    let routed = api
        .create_task(routed("routed-task-spec-identity", 10_000))
        .await
        .unwrap();
    let routed_binding = wait_for_binding(&api, routed.name(), 1).await;
    let routed_runtime_name = wait_for_taskvisor_name(&api, routed_binding.tv).await;
    assert_eq!(
        routed_runtime_name.as_ref(),
        allocated_name
            .lock()
            .as_deref()
            .expect("the runner received a RunId")
    );

    let embedded = api
        .create_embedded_task(
            embedded("embedded-task-spec-identity", 10_000),
            TaskFn::arc(|ctx: TaskContext| async move {
                ctx.cancelled().await;
                Err::<(), TaskError>(TaskError::Canceled)
            }),
        )
        .await
        .unwrap();
    let embedded_binding = wait_for_binding(&api, embedded.name(), 1).await;
    let embedded_runtime_name = wait_for_taskvisor_name(&api, embedded_binding.tv).await;
    assert!(
        embedded_runtime_name.starts_with("embedded-embedded-slot-"),
        "core must allocate a unique TaskSpec name for embedded tasks: {embedded_runtime_name}"
    );

    api.shutdown().await.unwrap();
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
        .create_embedded_task(embedded("embedded-resource", 1_000), immediate_task())
        .await
        .unwrap();
    assert_eq!(embedded_created.name().as_str(), "embedded-resource");
    assert_eq!(embedded_created.status().phase(), TaskPhase::Pending);
    wait_for_observed(&api, embedded_created.name(), 1).await;
    let embedded_applied = api
        .apply_embedded_task(embedded("embedded-resource", 2_000), immediate_task())
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
            cancellable_task(),
        )
        .await
        .unwrap();
    let first_binding = wait_for_binding(&api, first.name(), 1).await;

    let unchanged = api
        .apply_embedded_task(
            embedded_with_revision("embedded-revision", 10_000, "v1"),
            cancellable_task(),
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
            cancellable_task(),
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
        .create_embedded_task(routed("prebuilt-routed", 1_000), immediate_task())
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
async fn retained_task_limit_counts_routed_and_embedded_resources() {
    let config = StateConfig::new().try_with_max_retained_tasks(2).unwrap();
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_config(config)
        .start()
        .await
        .unwrap();

    api.create_task(routed("retained-routed", 1_000))
        .await
        .unwrap();
    api.create_embedded_task(embedded("retained-embedded", 1_000), immediate_task())
        .await
        .unwrap();

    assert!(matches!(
        api.create_embedded_task(embedded("rejected-at-limit", 1_000), immediate_task(),)
            .await,
        Err(CoreError::RetainedTaskLimitReached { limit: 2 })
    ));

    let applied = api
        .apply_task(routed("retained-routed", 2_000))
        .await
        .unwrap();
    assert_eq!(applied.metadata().generation(), 2);
    assert_eq!(api.query_tasks(&TaskQuery::new()).unwrap().items.len(), 2);

    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn retention_worker_does_not_reserve_a_resource_name_or_slot() {
    let api = api(RunnerRouter::new()).await;
    let sweep_name = TaskId::new("solti-state-sweep").unwrap();
    assert!(api.get_task(&sweep_name).is_none());

    api.create_embedded_task(embedded(sweep_name.as_str(), 1_000), immediate_task())
        .await
        .unwrap();
    api.create_embedded_task(retention_slot("former-sweep-slot"), immediate_task())
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
        .create_embedded_task(embedded("retained-briefly", 1_000), immediate_task())
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
        .create_embedded_task(embedded("conditional", 10_000), cancellable_task())
        .await
        .unwrap();
    wait_for_binding(&api, task.name(), task.metadata().generation()).await;

    assert!(
        api.query_task_runs_where(task.name(), &TaskRunQuery::new(), |_| false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        api.query_task_runs_where(task.name(), &TaskRunQuery::new(), |_| true)
            .await
            .unwrap()
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
        .query_task_runs_where(current.name(), &TaskRunQuery::new(), |gvk| {
            gvk.kind() != "Embedded"
        })
        .await
        .unwrap()
        .expect("the current parent is visible");
    assert_eq!(visible.items.len(), 1);
    assert_eq!(visible.items[0].generation(), 2);
    assert_eq!(visible.items[0].workload().kind(), "Subprocess");

    state.delete_task(current.name());
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn conditional_run_continuation_does_not_require_a_current_task() {
    let api = api(RunnerRouter::new()).await;
    let state = &api.reconciler.state;
    let task = state
        .create_desired(&routed("run-continuation-visibility", 1_000))
        .unwrap()
        .task;
    let resource = ResourceGeneration::from_task(&task);
    let tv = taskvisor::TaskId::for_tests();
    assert!(state.bind_tv(resource.clone(), tv));
    let binding = RuntimeBinding { resource, tv };
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));
    }

    let query = TaskRunQuery::new().with_limit(1);
    let first = api
        .query_task_runs_where(task.name(), &query, |_| true)
        .await
        .unwrap()
        .unwrap();
    let continuation = first.continuation.unwrap();
    assert!(state.delete_task(task.name()));

    let second = api
        .query_task_runs_where(task.name(), &query.with_continuation(continuation), |_| {
            true
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].attempt(), 2);

    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn conditional_apply_cannot_replace_a_hidden_existing_resource() {
    let api = api(RunnerRouter::new()).await;
    let embedded = api
        .create_embedded_task(embedded("hidden-apply", 10_000), cancellable_task())
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

#[solti_runner::async_trait]
impl Runner for PanicRunner {
    fn name(&self) -> &str {
        "panic"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
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
        .create_embedded_task(embedded("upgrade", 10_000), cancellable_task())
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
    release: tokio::sync::Notify,
}

impl BuildGate {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }

    async fn wait(&self) {
        let released = self.release.notified();
        self.started.store(true, Ordering::Release);
        released.await;
    }
}

struct FailOnceBlockingRunner {
    builds: Arc<AtomicUsize>,
    retry_gate: Arc<BuildGate>,
}

#[solti_runner::async_trait]
impl Runner for FailOnceBlockingRunner {
    fn name(&self) -> &str {
        "fail-once-blocking"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let build = self.builds.fetch_add(1, Ordering::AcqRel);
        if build == 0 {
            return Err(RunnerError::Internal("transient build failure".into()));
        }
        if build == 1 {
            self.retry_gate.wait().await;
        }
        Ok(immediate_task())
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

struct PredicateGate {
    started: AtomicBool,
    open: Mutex<bool>,
    changed: Condvar,
}

impl PredicateGate {
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
    let gate = Arc::new(PredicateGate::new());

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
            api.apply_embedded_task(embedded(name.as_str(), 2_000), immediate_task())
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
    build_finished: Arc<AtomicBool>,
    runtime_started: Arc<AtomicBool>,
}

struct BuildFinishedGuard(Arc<AtomicBool>);

impl Drop for BuildFinishedGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[solti_runner::async_trait]
impl Runner for BlockingRunner {
    fn name(&self) -> &str {
        "blocking"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let _finished = BuildFinishedGuard(Arc::clone(&self.build_finished));
        tokio::select! {
            _ = self.gate.wait() => {}
            _ = cancellation.cancelled() => {
                return Err(RunnerError::Internal("build cancelled".into()));
            }
        }
        let runtime_started = Arc::clone(&self.runtime_started);
        Ok(TaskFn::arc(move |_ctx: TaskContext| {
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
            build_finished: Arc::new(AtomicBool::new(false)),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_and_drains_blocked_runner_build() {
    let gate = Arc::new(BuildGate::new());
    let build_finished = Arc::new(AtomicBool::new(false));
    let runtime_started = Arc::new(AtomicBool::new(false));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(BlockingRunner {
            gate: Arc::clone(&gate),
            build_finished: Arc::clone(&build_finished),
            runtime_started: Arc::clone(&runtime_started),
        }))
        .unwrap();
    let api = api(router).await;
    let name = TaskId::new("shutdown-blocked-build").unwrap();

    api.create_task(routed(name.as_str(), 1_000)).await.unwrap();
    wait_for_build(&gate).await;

    tokio::time::timeout(Duration::from_secs(1), api.shutdown())
        .await
        .expect("shutdown must not wait for a blocked runner build")
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        while !build_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runner build future was not dropped during shutdown");
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        !runtime_started.load(Ordering::Acquire),
        "a late runner build result must not be submitted after shutdown"
    );
}

struct FirstBuildBlockingRunner {
    gate: Arc<BuildGate>,
    builds: AtomicUsize,
}

#[solti_runner::async_trait]
impl Runner for FirstBuildBlockingRunner {
    fn name(&self) -> &str {
        "first-build-blocking"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        if self.builds.fetch_add(1, Ordering::AcqRel) == 0 {
            tokio::select! {
                _ = self.gate.wait() => {}
                _ = cancellation.cancelled() => {
                    return Err(RunnerError::Internal("build cancelled".into()));
                }
            }
        }
        Ok(cancellable_task())
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

#[tokio::test]
async fn newer_apply_cancels_stale_preflight_waiting_for_the_runtime_lock() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(RecordingRunner {
            seen: Arc::clone(&seen),
        }))
        .unwrap();
    let api = api(router).await;
    let name = TaskId::new("cancel-after-build").unwrap();
    let runtime_operation = api.reconciler.runtime_operations.lock(&name).await;

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
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first runner build did not complete");

    let second = api.apply_task(routed(name.as_str(), 2_000)).await.unwrap();
    assert_eq!(second.metadata().generation(), 2);
    tokio::time::timeout(Duration::from_secs(2), first_done)
        .await
        .expect("stale preflight remained blocked on the runtime lock")
        .expect("stale reconciliation acknowledgement dropped");
    assert!(api.reconciler.state.binding_for(&name).is_none());

    drop(runtime_operation);
    wait_for_observed(&api, &name, 2).await;
    assert_eq!(
        seen.lock()
            .iter()
            .map(|(_, generation, _)| *generation)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn newer_apply_cancels_stale_submission_waiting_for_taskvisor_ownership() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_runtime_config(runtime)
        .start()
        .await
        .unwrap();

    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once("ownership-filler", cancellable_task()))
        .await
        .unwrap();
    let name = TaskId::new("cancel-ownership-intake").unwrap();

    let first = api
        .write(
            embedded_with_revision(name.as_str(), 10_000, "generation-1"),
            RuntimeSource::Prebuilt(cancellable_task()),
            WriteMode::Create,
            WritePreconditions::new(),
            true,
        )
        .await
        .unwrap();
    let first_done = first
        .reconciliation
        .expect("a created spec schedules reconciliation");
    let first_binding = wait_for_binding(&api, &name, 1).await;
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != first_binding.tv),
        "the first generation must still be waiting before controller intake"
    );

    let second = api
        .apply_embedded_task(
            embedded_with_revision(name.as_str(), 10_000, "generation-2"),
            cancellable_task(),
        )
        .await
        .unwrap();
    assert_eq!(second.metadata().generation(), 2);

    tokio::time::timeout(Duration::from_secs(2), first_done)
        .await
        .expect("stale ownership intake did not cancel")
        .expect("stale reconciliation acknowledgement dropped");
    let second_binding = wait_for_binding(&api, &name, 2).await;
    assert_ne!(
        second_binding.tv, first_binding.tv,
        "the newer generation must own a distinct prepared identity"
    );
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != second_binding.tv),
        "the newer generation must wait until ownership capacity is released"
    );
    let waiting = api
        .get_task(&name)
        .expect("newer desired state remains retained");
    assert_eq!(
        waiting.status().reconciled().status(),
        ConditionStatus::Unknown,
        "canceling stale intake must not report a reconciliation failure"
    );
    assert_eq!(waiting.status().observed_generation(), 0);
    assert_eq!(
        api.reconciler.output_hub.active_channels(),
        1,
        "the superseded pre-binding must not retain an output channel"
    );

    api.reconciler
        .handle
        .cancel_with_timeout(held_id, Duration::from_secs(1))
        .await
        .unwrap();
    let held_outcome = tokio::time::timeout(Duration::from_secs(1), held_waiter.wait())
        .await
        .expect("ownership filler did not finish")
        .expect("ownership filler outcome channel closed");
    assert_eq!(held_outcome.kind(), TaskOutcomeKind::Canceled);

    wait_for_observed(&api, &name, 2).await;
    assert_eq!(
        api.reconciler.state.binding_for(&name),
        Some(second_binding),
        "released capacity must admit the newer generation"
    );
    api.delete_task(&name).await.unwrap();
    api.shutdown().await.unwrap();
}

struct AdmissionProbe {
    active: AtomicUsize,
    entered: AtomicUsize,
    peak: AtomicUsize,
    release: tokio::sync::Semaphore,
}

impl AdmissionProbe {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            entered: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    async fn wait_for_entered(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.entered.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runner builds did not reach the admission probe");
    }
}

struct ActiveBuild(Arc<AdmissionProbe>);

impl Drop for ActiveBuild {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

struct AdmissionRunner {
    name: &'static str,
    probe: Arc<AdmissionProbe>,
}

#[solti_runner::async_trait]
impl Runner for AdmissionRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let active = self.probe.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.probe.peak.fetch_max(active, Ordering::AcqRel);
        self.probe.entered.fetch_add(1, Ordering::AcqRel);
        let _active = ActiveBuild(Arc::clone(&self.probe));
        let permit = tokio::select! {
            permit = self.probe.release.acquire() => {
                permit.expect("test admission semaphore remains open")
            }
            _ = cancellation.cancelled() => {
                return Err(RunnerError::Internal("build cancelled".into()));
            }
        };
        permit.forget();
        Ok(immediate_task())
    }
}

#[tokio::test]
async fn global_build_admission_never_exceeds_its_limit() {
    let probe = Arc::new(AdmissionProbe::new());
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(AdmissionRunner {
            name: "bounded",
            probe: Arc::clone(&probe),
        }))
        .unwrap();
    let config = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(2)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(2)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;
    let names = ["bounded-1", "bounded-2", "bounded-3", "bounded-4"];

    for name in names {
        api.create_task(routed(name, 1_000)).await.unwrap();
    }
    probe.wait_for_entered(2).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(probe.entered.load(Ordering::Acquire), 2);
    assert_eq!(probe.peak.load(Ordering::Acquire), 2);

    probe.release.add_permits(names.len());
    for name in names {
        wait_for_observed(&api, &TaskId::new(name).unwrap(), 1).await;
    }
    assert_eq!(probe.entered.load(Ordering::Acquire), names.len());
    assert_eq!(probe.peak.load(Ordering::Acquire), 2);
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn per_runner_admission_does_not_consume_another_runners_capacity() {
    let a = Arc::new(AdmissionProbe::new());
    let b = Arc::new(AdmissionProbe::new());
    let mut router = RunnerRouter::new();
    for (name, backend, probe) in [
        ("runner-a", "a", Arc::clone(&a)),
        ("runner-b", "b", Arc::clone(&b)),
    ] {
        let mut labels = Labels::new();
        labels.insert("backend", backend);
        router
            .register_with_labels(Arc::new(AdmissionRunner { name, probe }), labels)
            .unwrap();
    }
    let config = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(2)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(1)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;

    api.create_task(routed_to("a-1", 1_000, "a")).await.unwrap();
    api.create_task(routed_to("a-2", 1_000, "a")).await.unwrap();
    api.create_task(routed_to("b-1", 1_000, "b")).await.unwrap();
    a.wait_for_entered(1).await;
    b.wait_for_entered(1).await;
    assert_eq!(a.entered.load(Ordering::Acquire), 1);
    assert_eq!(a.peak.load(Ordering::Acquire), 1);
    assert_eq!(b.entered.load(Ordering::Acquire), 1);

    a.release.add_permits(2);
    b.release.add_permits(1);
    wait_for_observed(&api, &TaskId::new("a-1").unwrap(), 1).await;
    wait_for_observed(&api, &TaskId::new("a-2").unwrap(), 1).await;
    wait_for_observed(&api, &TaskId::new("b-1").unwrap(), 1).await;
    assert_eq!(a.entered.load(Ordering::Acquire), 2);
    assert_eq!(a.peak.load(Ordering::Acquire), 1);
    api.shutdown().await.unwrap();
}

struct SynchronizedChainRunner {
    name: &'static str,
    inner: ChainRunner,
    entered: Arc<AtomicUsize>,
    barrier: Arc<tokio::sync::Barrier>,
}

#[solti_runner::async_trait]
impl Runner for SynchronizedChainRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        self.inner.workload_types()
    }

    async fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        self.entered.fetch_add(1, Ordering::AcqRel);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(RunnerError::Internal("build cancelled".into()));
            }
            _ = self.barrier.wait() => {}
        }
        self.inner
            .build_task(task, run_id, ctx, cancellation, scope)
            .await
    }
}

fn one_step_chain(name: &str, backend: Option<&str>) -> TaskManifest {
    let step = ChainStep::new(
        "leaf",
        TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: vec![],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        )),
    )
    .unwrap();
    let workload = ChainSpec::new("leaf", vec![step])
        .unwrap()
        .into_workload()
        .unwrap();
    let mut builder = TaskSpec::builder(format!("{name}-slot"), workload, 1_000_u64);
    if let Some(backend) = backend {
        let mut labels = Labels::new();
        labels.insert("chain", backend);
        builder = builder.runner_selector(LabelSelector::from_labels(labels));
    }
    TaskManifest::new(name, builder.build().unwrap()).unwrap()
}

#[tokio::test]
async fn nested_leaf_builds_share_the_registered_runner_limit() {
    let leaf = Arc::new(AdmissionProbe::new());
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(AdmissionRunner {
            name: "leaf",
            probe: Arc::clone(&leaf),
        }))
        .unwrap();
    let catalog = router.catalog();
    let entered = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    for (name, backend) in [("chain-a", "a"), ("chain-b", "b")] {
        let mut labels = Labels::new();
        labels.insert("chain", backend);
        router
            .register_with_labels(
                Arc::new(SynchronizedChainRunner {
                    name,
                    inner: ChainRunner::new(name, catalog.clone()),
                    entered: Arc::clone(&entered),
                    barrier: Arc::clone(&barrier),
                }),
                labels,
            )
            .unwrap();
    }
    let config = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(2)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(1)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;

    api.create_task(one_step_chain("chain-task-a", Some("a")))
        .await
        .unwrap();
    api.create_task(one_step_chain("chain-task-b", Some("b")))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while entered.load(Ordering::Acquire) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both outer chain builds must enter concurrently");
    leaf.wait_for_entered(1).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert_eq!(leaf.entered.load(Ordering::Acquire), 1);
    assert_eq!(leaf.peak.load(Ordering::Acquire), 1);

    leaf.release.add_permits(1);
    leaf.wait_for_entered(2).await;
    assert_eq!(leaf.peak.load(Ordering::Acquire), 1);
    leaf.release.add_permits(1);
    wait_for_observed(&api, &TaskId::new("chain-task-a").unwrap(), 1).await;
    wait_for_observed(&api, &TaskId::new("chain-task-b").unwrap(), 1).await;
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_leaf_reuses_the_outer_global_slot() {
    let leaf = Arc::new(AdmissionProbe::new());
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(AdmissionRunner {
            name: "leaf",
            probe: Arc::clone(&leaf),
        }))
        .unwrap();
    let catalog = router.catalog();
    router
        .register(Arc::new(ChainRunner::new("chain", catalog)))
        .unwrap();
    let config = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(1)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(1)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;

    api.create_task(one_step_chain("single-global-chain", None))
        .await
        .unwrap();
    leaf.wait_for_entered(1).await;
    leaf.release.add_permits(1);
    wait_for_observed(&api, &TaskId::new("single-global-chain").unwrap(), 1).await;
    api.shutdown().await.unwrap();
}

struct CoalescingRunner {
    blocker: Arc<BuildGate>,
    builds: Arc<Mutex<Vec<(TaskId, u64)>>>,
}

struct DropProbeTask {
    dropped: Arc<AtomicBool>,
    gate_released: Arc<AtomicBool>,
    api: Weak<SupervisorApi>,
}

impl Drop for DropProbeTask {
    fn drop(&mut self) {
        let gate_released = self
            .api
            .upgrade()
            .is_some_and(|api| api.spawn_gate.try_lock().is_some());
        self.gate_released.store(gate_released, Ordering::Release);
        self.dropped.store(true, Ordering::Release);
    }
}

impl TvTask for DropProbeTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

#[solti_runner::async_trait]
impl Runner for CoalescingRunner {
    fn name(&self) -> &str {
        "coalescing"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        self.builds
            .lock()
            .push((task.name().clone(), task.metadata().generation()));
        if task.name().as_str() == "build-blocker" {
            tokio::select! {
                _ = self.blocker.wait() => {}
                _ = cancellation.cancelled() => {
                    return Err(RunnerError::Internal("build cancelled".into()));
                }
            }
        }
        Ok(immediate_task())
    }
}

#[tokio::test]
async fn pending_reconciliation_keeps_only_the_latest_generation() {
    let blocker = Arc::new(BuildGate::new());
    let builds = Arc::new(Mutex::new(Vec::new()));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(CoalescingRunner {
            blocker: Arc::clone(&blocker),
            builds: Arc::clone(&builds),
        }))
        .unwrap();
    let config = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(1)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(2)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;

    api.create_task(routed("build-blocker", 1_000))
        .await
        .unwrap();
    wait_for_build(&blocker).await;
    api.create_task(routed("coalesced", 1_000)).await.unwrap();
    api.apply_task(routed("coalesced", 2_000)).await.unwrap();
    api.apply_task(routed("coalesced", 3_000)).await.unwrap();

    blocker.release();
    wait_for_observed(&api, &TaskId::new("coalesced").unwrap(), 3).await;
    assert_eq!(
        builds.lock().as_slice(),
        [
            (TaskId::new("build-blocker").unwrap(), 1),
            (TaskId::new("coalesced").unwrap(), 3),
        ]
    );
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn coalescing_defers_user_task_destruction_to_the_caller_boundary() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let manifest = embedded("coalesced-drop", 1_000);
    let desired = Task::from_manifest(manifest.clone()).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let gate_released = Arc::new(AtomicBool::new(false));
    let registration_tracker = TaskTracker::new();
    let registration = registration_tracker.token();
    assert_eq!(registration_tracker.len(), 1);
    let operation = api.task_operations.lock(desired.name()).await;

    let (_first_completion, first_superseded) = api.reconciler.schedule(
        desired.clone(),
        RuntimeSource::Prebuilt(Arc::new(DropProbeTask {
            dropped: Arc::clone(&dropped),
            gate_released: Arc::clone(&gate_released),
            api: Arc::downgrade(&api),
        })),
        true,
        registration,
    );
    assert!(first_superseded.is_none());

    let scheduled = api
        .write_locked(
            manifest,
            RuntimeSource::Prebuilt(immediate_task()),
            WriteMode::Apply,
            &WritePreconditions::new(),
            true,
            operation,
        )
        .unwrap();
    drop(scheduled);
    assert_eq!(registration_tracker.len(), 0);
    assert!(dropped.load(Ordering::Acquire));
    assert!(gate_released.load(Ordering::Acquire));
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn schedule_returns_superseded_source_without_dropping_it() {
    let api = api(RunnerRouter::new()).await;
    let desired = Task::from_manifest(embedded("coalesced-return", 1_000)).unwrap();
    let dropped = Arc::new(AtomicBool::new(false));

    let (_first_completion, first_superseded) = api.reconciler.schedule(
        desired.clone(),
        RuntimeSource::Prebuilt(Arc::new(DropProbeTask {
            dropped: Arc::clone(&dropped),
            gate_released: Arc::new(AtomicBool::new(false)),
            api: Weak::new(),
        })),
        true,
        api.reconciler.tasks.token(),
    );
    assert!(first_superseded.is_none());

    let (_second_completion, superseded) = api.reconciler.schedule(
        desired,
        RuntimeSource::Prebuilt(immediate_task()),
        true,
        api.reconciler.tasks.token(),
    );
    let superseded = superseded.expect("the unpolled pending request is replaced");
    assert!(!dropped.load(Ordering::Acquire));

    drop(superseded);
    assert!(dropped.load(Ordering::Acquire));
    api.shutdown().await.unwrap();
}

struct DeadlineRunner {
    dropped: Arc<AtomicBool>,
}

#[solti_runner::async_trait]
impl Runner for DeadlineRunner {
    fn name(&self) -> &str {
        "deadline"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        subprocess_workload_types()
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let _dropped = BuildFinishedGuard(Arc::clone(&self.dropped));
        cancellation.cancelled().await;
        Err(RunnerError::Internal("build cancelled".into()))
    }
}

#[tokio::test]
async fn build_deadline_cancels_and_drops_the_runner_future() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(DeadlineRunner {
            dropped: Arc::clone(&dropped),
        }))
        .unwrap();
    let config = ReconciliationConfig::new()
        .try_with_build_timeout(Duration::from_millis(25))
        .unwrap();
    let api = api_with_reconciliation(router, config).await;

    let task = api.create_task(routed("deadline", 1_000)).await.unwrap();
    let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;
    assert_eq!(failed.status().reconciled().reason(), "RunnerBuildTimedOut");
    assert!(dropped.load(Ordering::Acquire));
    assert!(api.reconciler.state.binding_for(task.name()).is_none());
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn build_deadline_includes_root_admission_wait() {
    let probe = Arc::new(AdmissionProbe::new());
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(AdmissionRunner {
            name: "admission-deadline",
            probe: Arc::clone(&probe),
        }))
        .unwrap();
    let config = ReconciliationConfig::new()
        .try_with_build_timeout(Duration::from_millis(100))
        .unwrap()
        .try_with_max_concurrent_builds(1)
        .unwrap()
        .try_with_max_concurrent_builds_per_runner(1)
        .unwrap();
    let api = api_with_reconciliation(router, config).await;
    let held_task = Task::from_manifest(routed("held-admission", 1_000)).unwrap();
    let held_admission = api.reconciler.admit_for_test(&held_task).await.unwrap();

    let task = api
        .create_task(routed("admission-timeout", 1_000))
        .await
        .unwrap();
    let failed = wait_for_reconciled(&api, task.name(), 1, ConditionStatus::False).await;

    assert_eq!(failed.status().reconciled().reason(), "RunnerBuildTimedOut");
    assert_eq!(probe.entered.load(Ordering::Acquire), 0);
    drop(held_admission);

    api.create_task(routed("admission-recovery", 1_000))
        .await
        .unwrap();
    probe.wait_for_entered(1).await;
    probe.release.add_permits(1);
    wait_for_observed(&api, &TaskId::new("admission-recovery").unwrap(), 1).await;
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_during_blocked_preflight_prevents_late_runtime_submission() {
    let gate = Arc::new(BuildGate::new());
    let build_finished = Arc::new(AtomicBool::new(false));
    let runtime_started = Arc::new(AtomicBool::new(false));
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(BlockingRunner {
            gate: Arc::clone(&gate),
            build_finished: Arc::clone(&build_finished),
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

    tokio::time::timeout(Duration::from_secs(2), reconciliation)
        .await
        .expect("stale reconciliation did not finish")
        .expect("stale reconciliation acknowledgement dropped");
    assert!(build_finished.load(Ordering::Acquire));
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
        .create_embedded_task(embedded("too-late", 1_000), immediate_task())
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
