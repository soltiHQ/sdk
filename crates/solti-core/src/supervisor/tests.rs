use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};

use parking_lot::{Condvar, Mutex};
use solti_chain::{ChainRunner, ChainSpec, ChainStep};
use solti_model::{
    AdmissionPolicy, ConditionStatus, EmbeddedSpec, Flag, LabelSelector, Labels, Slot,
    SubprocessMode, SubprocessSpec, TaskEnv, TaskPhase, TaskSpec, TaskWorkload, Uid,
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
use crate::state::{TASKVISOR_INTAKE_PENDING_MESSAGE, TASKVISOR_INTAKE_PENDING_REASON};
use crate::{
    PersistenceConfig, ReconciliationConfig, StateConfig, TaskOutputEvent, TaskOutputSink,
    TaskStateEvent, TaskStateSink,
};

struct TokioDependentStateSink {
    first: AtomicBool,
    events: AtomicUsize,
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl TaskStateSink for TokioDependentStateSink {
    fn on_event(&self, _event: &TaskStateEvent) {
        self.events.fetch_add(1, Ordering::AcqRel);
        if self.first.swap(false, Ordering::AcqRel) {
            self.entered
                .send(())
                .expect("the test must observe the active persistence callback");
            self.release
                .lock()
                .recv()
                .expect("ordinary Tokio work must release the persistence callback");
        }
    }
}

struct IgnoringStateSink;

impl TaskStateSink for IgnoringStateSink {
    fn on_event(&self, _event: &TaskStateEvent) {}
}

struct IgnoringOutputSink;

impl TaskOutputSink for IgnoringOutputSink {
    fn on_event(&self, _event: &TaskOutputEvent) {}
}

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

struct PanickingDropPayload;

impl Drop for PanickingDropPayload {
    fn drop(&mut self) {
        panic!("nested panic payload destructor");
    }
}

struct PanickingDropTask {
    dropped: Arc<AtomicBool>,
}

impl Drop for PanickingDropTask {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        std::panic::panic_any(PanickingDropPayload);
    }
}

impl TvTask for PanickingDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

fn panicking_drop_task(dropped: Arc<AtomicBool>) -> TaskRef {
    Arc::new(PanickingDropTask { dropped })
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

async fn spawn_registered_delete(
    api: &Arc<SupervisorApi>,
    name: &TaskId,
) -> tokio::task::JoinHandle<Result<(), CoreError>> {
    let tracked_before = api.delete_operations.len();
    let delete_api = Arc::clone(api);
    let delete_name = name.clone();
    let deletion = tokio::spawn(async move { delete_api.delete_task(&delete_name).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.delete_operations.len() <= tracked_before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete did not register its SDK-owned worker");
    deletion
}

async fn wait_for_deleted(api: &SupervisorApi, name: &TaskId) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.get_task(name).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the SDK-owned delete worker did not remove desired state");
}

#[tokio::test(flavor = "current_thread")]
async fn public_write_yields_when_state_persistence_capacity_is_full() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(true),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(sink)
        .with_persistence_config(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(2)
                .unwrap(),
        )
        .start()
        .await
        .unwrap();

    api.reconciler
        .state
        .add_task(embedded("persistence-active", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the first callback must become active");
    api.reconciler
        .state
        .add_task(embedded("persistence-buffered-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("persistence-buffered-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

    // The watchdog only prevents a broken synchronous implementation from
    // hanging the suite forever. The asserted path releases from Tokio work.
    let watchdog_release = release_tx.clone();
    let (completed_tx, completed_rx) = mpsc::sync_channel(1);
    let watchdog = std::thread::spawn(move || {
        if completed_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            let _ = watchdog_release.try_send(());
        }
    });
    let release = tokio::spawn(async move {
        tokio::task::yield_now().await;
        release_tx
            .try_send(())
            .expect("the Tokio task owns the callback release");
    });

    let created = tokio::time::timeout(
        Duration::from_secs(1),
        api.create_embedded_task(embedded("public-write", 1_000), immediate_task()),
    )
    .await;
    let _ = completed_tx.send(());
    watchdog.join().unwrap();
    release.await.unwrap();
    created
        .expect("public state admission must yield to ordinary Tokio work")
        .unwrap();

    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_timeout_returns_while_the_owned_coordinator_keeps_draining() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(true),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(sink.clone())
        .start()
        .await
        .unwrap();
    api.reconciler
        .state
        .add_task(embedded("shutdown-deadline", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the persistence callback must block before shutdown");

    let timeout = Duration::from_millis(25);
    assert!(matches!(
        api.shutdown_with_timeout(timeout).await,
        Err(CoreError::ShutdownTimedOut { timeout: actual }) if actual == timeout
    ));
    assert!(api.shutdown_started.load(Ordering::Acquire));
    let first_operation = api
        .shutdown
        .operation
        .lock()
        .as_ref()
        .cloned()
        .expect("the first shutdown waiter must install one shared operation");

    let second_shutdown = api.shutdown();
    tokio::pin!(second_shutdown);
    assert!(
        tokio::time::timeout(timeout, &mut second_shutdown)
            .await
            .is_err(),
        "a later public shutdown waiter must observe the same pending drain"
    );
    let second_operation = api
        .shutdown
        .operation
        .lock()
        .as_ref()
        .cloned()
        .expect("the later waiter must join the installed operation");
    assert!(
        Arc::ptr_eq(&first_operation, &second_operation),
        "all public waiters must share one SDK-owned shutdown operation"
    );

    release_tx
        .send(())
        .expect("the callback must resume after the caller observed timeout");
    tokio::time::timeout(Duration::from_secs(5), &mut second_shutdown)
        .await
        .expect("the later public shutdown waiter must finish after callback release")
        .unwrap();
    assert_eq!(sink.events.load(Ordering::Acquire), 1);
    let status = api.state_persistence_status().unwrap();
    assert!(!status.accepting());
    assert_eq!(status.queued(), 0);
    assert_eq!(status.queued_bytes(), 0);
    assert_eq!(status.delivered(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_worker_panic_does_not_skip_the_state_persistence_cleanup_tail() {
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(Arc::new(IgnoringStateSink))
        .with_output_sink(Arc::new(IgnoringOutputSink))
        .start()
        .await
        .unwrap();
    api.reconciler.output_hub.inject_persistence_worker_panic();
    api.reconciler.output_hub.announce_run_started(
        &TaskId::new("output-worker-panic").unwrap(),
        &Uid::new("output-worker-panic-uid").unwrap(),
        1,
        1,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while api.output_persistence_status().unwrap().healthy() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the injected output worker panic did not become observable");
    assert!(api.state_persistence_status().unwrap().accepting());

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), api.shutdown()).await,
        Ok(Err(CoreError::ShutdownCoordinatorStopped))
    ));
    let state_status = api.state_persistence_status().unwrap();
    assert!(!state_status.accepting());
    assert_eq!(state_status.queued(), 0);
    assert_eq!(state_status.queued_bytes(), 0);

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), api.shutdown()).await,
        Ok(Err(CoreError::ShutdownCoordinatorStopped))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_worker_panic_does_not_skip_the_output_persistence_cleanup_tail() {
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(Arc::new(IgnoringStateSink))
        .with_output_sink(Arc::new(IgnoringOutputSink))
        .start()
        .await
        .unwrap();
    api.reconciler.state.inject_persistence_worker_panic();
    api.reconciler
        .state
        .add_task(embedded("state-worker-panic", 1_000));
    tokio::time::timeout(Duration::from_secs(5), async {
        while api.state_persistence_status().unwrap().healthy() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the injected state worker panic did not become observable");
    assert!(api.output_persistence_status().unwrap().accepting());

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), api.shutdown()).await,
        Ok(Err(CoreError::ShutdownCoordinatorStopped))
    ));
    let output_status = api.output_persistence_status().unwrap();
    assert!(!output_status.accepting());
    assert_eq!(output_status.queued(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_public_shutdown_waiter_does_not_cancel_the_owned_drain() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(true),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_state_sink(sink.clone())
            .start()
            .await
            .unwrap(),
    );
    api.reconciler
        .state
        .add_task(embedded("aborted-shutdown-waiter", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the persistence callback must block before shutdown");

    let first_api = Arc::clone(&api);
    let first_waiter = tokio::spawn(async move { first_api.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !api.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first public shutdown waiter must start shutdown");
    first_waiter.abort();
    assert!(first_waiter.await.unwrap_err().is_cancelled());

    let second_api = Arc::clone(&api);
    let mut second_waiter = tokio::spawn(async move { second_api.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut second_waiter)
            .await
            .is_err(),
        "aborting one caller must not complete the still-blocked owned drain"
    );

    release_tx
        .send(())
        .expect("the callback must resume after the second waiter is pending");
    tokio::time::timeout(Duration::from_secs(5), second_waiter)
        .await
        .expect("the second public shutdown waiter must finish after callback release")
        .unwrap()
        .unwrap();

    assert_eq!(sink.events.load(Ordering::Acquire), 1);
    let status = api.state_persistence_status().unwrap();
    assert!(!status.accepting());
    assert_eq!(status.queued(), 0);
    assert_eq!(status.queued_bytes(), 0);
    assert_eq!(status.delivered(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_persistence_capacity_preserves_an_accepted_delete_during_shutdown() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(true),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_state_sink(sink.clone())
            .with_persistence_config(
                PersistenceConfig::new()
                    .try_with_state_queue_capacity(2)
                    .unwrap(),
            )
            .start()
            .await
            .unwrap(),
    );
    let target = TaskId::new("delete-during-shutdown").unwrap();

    api.reconciler
        .state
        .add_task(embedded(target.as_str(), 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the first persistence callback must become active");
    api.reconciler
        .state
        .add_task(embedded("delete-buffered-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("delete-buffered-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

    let delete_api = Arc::clone(&api);
    let delete_target = target.clone();
    let deletion = tokio::spawn(async move { delete_api.delete_task(&delete_target).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while api.reconciler.state.persistence_admission_waiters() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete admission must be pending after cloning the persistence sender");

    let shutdown_api = Arc::clone(&api);
    let shutdown = tokio::spawn(async move { shutdown_api.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !api.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown must close operation admission");
    assert!(
        !api.reconciler.state.persistence_admission_closed(),
        "shutdown must keep persistence admission open while the owned delete drains"
    );
    assert!(!deletion.is_finished());
    assert!(!shutdown.is_finished());

    release_tx
        .send(())
        .expect("the test must release the active persistence callback");
    tokio::time::timeout(Duration::from_secs(5), deletion)
        .await
        .expect("the accepted delete must finish")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown must drain the accepted delete event")
        .unwrap()
        .unwrap();

    assert!(api.get_task(&target).is_none());
    assert_eq!(sink.events.load(Ordering::Acquire), 4);
    assert_eq!(api.state_persistence_status().unwrap().delivered(), 4);
    assert_eq!(api.state_persistence_status().unwrap().queued(), 0);
    assert!(api.reconciler.state.persistence_admission_closed());
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
async fn maximum_checked_forward_deadlines_start_without_overflow() {
    let maximum = Duration::from_secs(60 * 60 * 24 * 365 * 30);
    let state = StateConfig::new().try_with_sweep_interval(maximum).unwrap();
    let reconciliation = ReconciliationConfig::new()
        .try_with_build_timeout(maximum)
        .unwrap();
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_config(state)
        .with_reconciliation_config(reconciliation)
        .start()
        .await
        .unwrap();

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
async fn conditional_per_id_operations_share_visibility_and_the_resource_operation_lock() {
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
        api.subscribe_output_where(task.name(), task.uid(), |_| false)
            .await
            .is_none()
    );
    assert!(
        api.subscribe_output_where(task.name(), &Uid::new("wrong-uid").unwrap(), |_| true)
            .await
            .is_none()
    );
    let (generation, _subscription) = api
        .subscribe_output_where(task.name(), task.uid(), |_| true)
        .await
        .expect("current bound generation has an output channel");
    assert_eq!(generation, task.metadata().generation());

    assert!(matches!(
        api.cancel_task_where(task.name(), WritePreconditions::new(), |_| false)
            .await,
        Err(CoreError::NotFound(_))
    ));
    assert!(api.get_task(task.name()).is_some());
    assert!(matches!(
        api.cancel_task_where(
            &TaskId::new("missing").unwrap(),
            WritePreconditions::new(),
            |_| true,
        )
        .await,
        Err(CoreError::NotFound(_))
    ));
    api.cancel_task_where(task.name(), WritePreconditions::new(), |_| true)
        .await
        .unwrap();
    assert!(api.get_task(task.name()).is_some());

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
async fn checked_cancel_rejects_stale_uid_and_retains_desired_state() {
    let api = api(RunnerRouter::new()).await;
    let task = api
        .create_task(routed("checked-cancel", 1_000))
        .await
        .unwrap();
    let stale = WritePreconditions::new().with_uid(solti_model::Uid::new("stale-uid").unwrap());

    let error = api
        .cancel_task_with_preconditions(task.name(), stale)
        .await
        .unwrap_err();

    assert!(matches!(error, CoreError::Conflict(_)));
    let current = api.get_task(task.name()).expect("task remains retained");

    let matching = WritePreconditions::new().with_uid(current.uid().clone());
    api.cancel_task_with_preconditions(task.name(), matching)
        .await
        .unwrap();
    assert!(api.get_task(task.name()).is_some());

    api.delete_task(task.name()).await.unwrap();
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
    let first_transition_time = first.committed.status().reconciled().last_transition_time();
    let first_done = first
        .reconciliation
        .expect("a created spec schedules reconciliation");
    let first_binding = wait_for_binding(&api, &name, 1).await;
    let first_waiting = wait_for_task(&api, &name, |task| {
        let condition = task.status().reconciled();
        condition.observed_generation() == 1
            && condition.status() == ConditionStatus::Unknown
            && condition.reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;
    assert_eq!(
        first_waiting.status().reconciled().message(),
        TASKVISOR_INTAKE_PENDING_MESSAGE
    );
    assert_eq!(
        first_waiting.status().reconciled().last_transition_time(),
        first_transition_time,
        "changing an Unknown diagnostic must preserve the status transition time"
    );
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
    let second_transition_time = second.status().reconciled().last_transition_time();

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
    let waiting = wait_for_task(&api, &name, |task| {
        let condition = task.status().reconciled();
        condition.observed_generation() == 2
            && condition.status() == ConditionStatus::Unknown
            && condition.reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;
    assert_eq!(
        waiting.status().reconciled().status(),
        ConditionStatus::Unknown,
        "canceling stale intake must not report a reconciliation failure"
    );
    assert_eq!(
        waiting.status().reconciled().message(),
        TASKVISOR_INTAKE_PENDING_MESSAGE
    );
    assert_eq!(
        waiting.status().reconciled().last_transition_time(),
        second_transition_time,
        "changing an Unknown diagnostic must preserve the status transition time"
    );
    let waiting_json = serde_json::to_value(&waiting).unwrap();
    let reconciled = &waiting_json["status"]["conditions"][0];
    assert_eq!(reconciled["status"], "Unknown");
    assert_eq!(reconciled["observedGeneration"], 2);
    assert_eq!(reconciled["reason"], TASKVISOR_INTAKE_PENDING_REASON);
    assert_eq!(reconciled["message"], TASKVISOR_INTAKE_PENDING_MESSAGE);
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

    let observed = wait_for_observed(&api, &name, 2).await;
    assert_eq!(observed.status().reconciled().reason(), "RuntimeAccepted");
    assert_eq!(
        api.reconciler.state.binding_for(&name),
        Some(second_binding),
        "released capacity must admit the newer generation"
    );
    api.delete_task(&name).await.unwrap();
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_task_releases_saturated_taskvisor_intake_and_allows_exact_retry() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_runtime_config(runtime)
        .start()
        .await
        .unwrap();

    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "cancel-task-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("cancel-task-saturated-intake").unwrap();
    let manifest = embedded(name.as_str(), 10_000);
    let created = api
        .create_embedded_task(manifest.clone(), cancellable_task())
        .await
        .unwrap();
    let prepared_binding = wait_for_binding(&api, &name, 1).await;
    wait_for_task(&api, &name, |task| {
        task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;

    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("cancel_task must not wait for saturated Taskvisor ownership")
        .unwrap();

    let cancelled = api
        .get_task(&name)
        .expect("explicit cancellation retains desired state");
    assert_eq!(cancelled.uid(), created.uid());
    assert_eq!(cancelled.metadata().generation(), 1);
    assert_eq!(cancelled.status().phase(), TaskPhase::Pending);
    assert_eq!(cancelled.status().attempt(), 0);
    assert_eq!(
        cancelled.status().reconciled().status(),
        ConditionStatus::False
    );
    assert_eq!(
        cancelled.status().reconciled().reason(),
        "RuntimeSubmissionCancelled"
    );
    assert_eq!(
        cancelled.status().reconciled().message(),
        "runtime submission was cancelled before Taskvisor intake completed"
    );
    assert_eq!(cancelled.status().observed_generation(), 1);
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != prepared_binding.tv),
        "a dropped PreparedSubmission must never reach Taskvisor"
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items
            .len(),
        0,
        "pre-intake cancellation must not invent an attempt"
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        api.reconciler.runtime_operations.lock(&name),
    )
    .await
    .expect("cancelled reconciliation must not leak the per-task runtime lock");

    let retried = api
        .apply_embedded_task(manifest, cancellable_task())
        .await
        .unwrap();
    assert_eq!(retried.metadata().generation(), 1);
    assert_eq!(
        retried.status().reconciled().status(),
        ConditionStatus::Unknown
    );
    let retry_binding = wait_for_binding(&api, &name, 1).await;
    assert_ne!(retry_binding.tv, prepared_binding.tv);

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

    wait_for_observed(&api, &name, 1).await;
    api.delete_task(&name).await.unwrap();
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn panicking_last_owner_during_saturated_intake_cannot_strand_cancel_or_shutdown() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_runtime_config(runtime)
        .start()
        .await
        .unwrap();
    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "panic-drop-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();

    let name = TaskId::new("panic-drop-saturated-intake").unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    api.create_embedded_task(
        embedded(name.as_str(), 10_000),
        panicking_drop_task(Arc::clone(&dropped)),
    )
    .await
    .unwrap();
    let provisional = wait_for_binding(&api, &name, 1).await;
    wait_for_task(&api, &name, |task| {
        task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;

    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("panicking pre-intake disposal must not strand cancel_task")
        .unwrap();

    assert!(dropped.load(Ordering::Acquire));
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != provisional.tv),
        "the provisional Taskvisor identity must remain unsubmitted"
    );
    let cancelled = api.get_task(&name).unwrap();
    assert_eq!(cancelled.status().phase(), TaskPhase::Pending);
    assert_eq!(cancelled.status().attempt(), 0);
    assert_eq!(
        cancelled.status().reconciled().reason(),
        "RuntimeSubmissionCancelled"
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items
            .len(),
        0
    );

    api.reconciler
        .handle
        .cancel_with_timeout(held_id, Duration::from_secs(1))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), held_waiter.wait())
        .await
        .expect("ownership filler did not finish")
        .expect("ownership filler outcome channel closed");
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain after panicking pre-intake disposal")
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_last_owner_before_preparation_does_not_escape_public_write() {
    let api = api(RunnerRouter::new()).await;
    let name = TaskId::new("panic-drop-invalid-source").unwrap();
    let dropped = Arc::new(AtomicBool::new(false));

    let result = api
        .create_embedded_task(
            routed(name.as_str(), 1_000),
            panicking_drop_task(Arc::clone(&dropped)),
        )
        .await;
    assert!(matches!(result, Err(CoreError::InvalidSpec(_))));
    assert!(dropped.load(Ordering::Acquire));
    assert!(api.get_task(&name).is_none());
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.reconciler.handle.list().await.is_empty());
    assert!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain after rejected source disposal")
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_last_owner_in_pending_source_cannot_strand_cancel_or_shutdown() {
    let api = api(RunnerRouter::new()).await;
    let name = TaskId::new("panic-drop-pending-source").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), immediate_task())
        .await
        .unwrap();

    let dropped = Arc::new(AtomicBool::new(false));
    let applied = api
        .apply_embedded_task(
            embedded_with_revision(name.as_str(), 10_000, "pending-v2"),
            panicking_drop_task(Arc::clone(&dropped)),
        )
        .await
        .unwrap();
    assert_eq!(applied.metadata().generation(), 2);
    let settled = api
        .reconciler
        .cancel_scheduled_for_user(&name)
        .expect("the unpolled pending source must still be scheduled");

    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("pending-source disposal must not strand cancel_task")
        .unwrap();
    assert!(settled.is_cancelled());
    assert!(dropped.load(Ordering::Acquire));
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.reconciler.handle.list().await.is_empty());
    let cancelled = api.get_task(&name).unwrap();
    assert_eq!(cancelled.metadata().generation(), 2);
    assert_eq!(cancelled.status().phase(), TaskPhase::Pending);
    assert_eq!(cancelled.status().attempt(), 0);
    assert_eq!(
        cancelled.status().reconciled().reason(),
        "RuntimeSubmissionCancelled"
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items
            .len(),
        0
    );
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain after pending-source disposal")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_bind_panic_releases_the_exact_unsubmitted_binding_before_settlement() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("panic-after-provisional-bind").unwrap();
    let (entered, release) = api
        .reconciler
        .arm_after_provisional_bind_panic(name.clone());

    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("reconciliation did not reach the post-bind panic point")
        .expect("post-bind panic point was dropped");
    let provisional = api
        .reconciler
        .state
        .binding_for(&name)
        .expect("the injected panic must pause with an exact provisional binding");
    assert!(api.subscribe_output(&name).is_some());

    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !cancellation.is_finished(),
        "cancel_task must wait for coordinator-owned unwind recovery"
    );
    release
        .send(())
        .expect("the injected reconciliation panic must still be armed");

    tokio::time::timeout(Duration::from_secs(2), cancellation)
        .await
        .expect("post-bind panic stranded cancel_task")
        .expect("cancel_task worker panicked")
        .unwrap();
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.subscribe_output(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != provisional.tv),
        "the provisional identity must never enter Taskvisor"
    );
    let retained = api
        .get_task(&name)
        .expect("cancellation retains desired state");
    assert_eq!(retained.status().phase(), TaskPhase::Pending);
    assert_eq!(retained.status().attempt(), 0);
    assert_eq!(
        retained.status().reconciled().reason(),
        "RuntimeSubmissionCancelled"
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items
            .len(),
        0,
        "unsubmitted unwind recovery must not invent a Taskvisor attempt"
    );
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain after post-bind unwind recovery")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_handoff_panic_preserves_the_authoritative_taskvisor_waiter() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("panic-before-accepted-waiter-handoff").unwrap();
    let (entered, release) = api
        .reconciler
        .arm_before_accepted_waiter_handoff_panic(name.clone());

    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("reconciliation did not reach the accepted handoff panic point")
        .expect("accepted handoff panic point was dropped");
    let binding = api
        .reconciler
        .state
        .binding_for(&name)
        .expect("accepted runtime must retain its exact binding");
    wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Running && task.status().attempt() == 1
    })
    .await;
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .any(|(id, _)| *id == binding.tv),
        "Taskvisor must own the accepted identity before cancellation"
    );

    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !cancellation.is_finished(),
        "cancel_task must wait for the accepted waiter recovery handoff"
    );
    release
        .send(())
        .expect("the accepted handoff panic must still be armed");

    tokio::time::timeout(Duration::from_secs(2), cancellation)
        .await
        .expect("accepted handoff panic stranded exact-ID cancellation")
        .expect("cancel_task worker panicked")
        .unwrap();
    let cancelled = api
        .get_task(&name)
        .expect("Taskvisor cancellation retains desired state");
    assert_eq!(cancelled.status().phase(), TaskPhase::Canceled);
    assert_eq!(cancelled.status().attempt(), 1);
    assert_eq!(
        cancelled.status().reconciled().status(),
        ConditionStatus::True,
        "accepted work keeps Taskvisor's authoritative reconciliation outcome"
    );
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.subscribe_output(&name).is_none());
    let runs = api
        .query_task_runs(&name, &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].attempt(), 1);
    assert_eq!(runs.items[0].phase(), TaskPhase::Canceled);
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain the recovered authoritative waiter")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_registration_panic_keeps_accepted_cancellation_taskvisor_native() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("panic-after-accepted-waiter-registration").unwrap();
    let (entered, release) = api
        .reconciler
        .arm_after_accepted_waiter_registration_panic(name.clone());

    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("reconciliation did not reach the post-registration panic point")
        .expect("post-registration panic point was dropped");
    let binding = api
        .reconciler
        .state
        .binding_for(&name)
        .expect("accepted runtime must retain its exact binding");
    wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Running && task.status().attempt() == 1
    })
    .await;

    release
        .send(())
        .expect("the post-registration panic must still be armed");
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.cancel_scheduled_for_user(&name).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent coordinator did not consume accepted progress after child panic");
    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("post-registration panic stranded exact-ID cancellation")
        .unwrap();

    let cancelled = api.get_task(&name).unwrap();
    assert_eq!(cancelled.status().phase(), TaskPhase::Canceled);
    assert_eq!(cancelled.status().attempt(), 1);
    assert_eq!(
        cancelled.status().reconciled().status(),
        ConditionStatus::True
    );
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );
    let runs = api
        .query_task_runs(&name, &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].phase(), TaskPhase::Canceled);
    tokio::time::timeout(Duration::from_secs(2), api.shutdown())
        .await
        .expect("shutdown must drain after post-registration panic recovery")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_cancel_task_before_intake_releases_ownership_before_status_admission() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(false),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_runtime_config(runtime)
            .with_state_sink(sink.clone())
            .with_persistence_config(
                PersistenceConfig::new()
                    .try_with_state_queue_capacity(2)
                    .unwrap(),
            )
            .start()
            .await
            .unwrap(),
    );

    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "aborted-cancel-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("aborted-cancel-before-intake").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    let prepared_binding = wait_for_binding(&api, &name, 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if api.get_task(&name).is_some_and(|task| {
                task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Taskvisor intake-pending state did not converge");

    tokio::time::timeout(Duration::from_secs(5), async {
        while api.state_persistence_status().unwrap().queued() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial desired and intake-pending events did not drain");
    sink.first.store(true, Ordering::Release);
    api.reconciler
        .state
        .add_task(embedded("aborted-cancel-persistence-active", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the armed persistence callback must become active");

    api.reconciler
        .state
        .add_task(embedded("aborted-cancel-persistence-buffer-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("aborted-cancel-persistence-buffer-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.state.persistence_admission_waiters() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cancellation status write must wait for persistence admission");

    cancellation.abort();
    assert!(
        cancellation.await.unwrap_err().is_cancelled(),
        "aborting the caller must stop only its wait"
    );
    assert!(
        api.reconciler.state.binding_for(&name).is_none(),
        "the prepared binding must be released before status persistence admission"
    );
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != prepared_binding.tv),
        "the cancelled prepared identity must never enter Taskvisor"
    );

    let handle = api.reconciler.handle.clone();
    let probe = tokio::spawn(async move {
        handle
            .add_and_watch(TvTaskSpec::once(
                "aborted-cancel-ownership-probe",
                cancellable_task(),
            ))
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !probe.is_finished(),
        "the ownership probe must wait while the filler still owns capacity"
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
    let (probe_id, probe_waiter) = tokio::time::timeout(Duration::from_secs(1), probe)
        .await
        .expect("released ownership must admit the independent probe")
        .unwrap()
        .unwrap();
    assert!(
        api.reconciler.state.persistence_admission_waiters() >= 1,
        "status persistence must still have a blocked admission after ownership is released"
    );

    release_tx
        .send(())
        .expect("the test must release the active persistence callback");
    let cancelled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(task) = api.get_task(&name)
                && task.status().reconciled().reason() == "RuntimeSubmissionCancelled"
            {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancellation status did not converge after persistence resumed");
    assert_eq!(cancelled.status().phase(), TaskPhase::Pending);
    assert_eq!(cancelled.status().attempt(), 0);
    assert_eq!(
        cancelled.status().reconciled().status(),
        ConditionStatus::False
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items
            .len(),
        0,
        "pre-intake cancellation must not invent a Taskvisor outcome"
    );

    api.reconciler
        .handle
        .cancel_with_timeout(probe_id, Duration::from_secs(1))
        .await
        .unwrap();
    let probe_outcome = tokio::time::timeout(Duration::from_secs(1), probe_waiter.wait())
        .await
        .expect("ownership probe did not finish")
        .expect("ownership probe outcome channel closed");
    assert_eq!(probe_outcome.kind(), TaskOutcomeKind::Canceled);
    api.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_task_delegates_an_accepted_runtime_to_taskvisor() {
    let api = api(RunnerRouter::new()).await;
    let name = TaskId::new("cancel-task-accepted-runtime").unwrap();
    let manifest = embedded(name.as_str(), 10_000);
    let created = api
        .create_embedded_task(manifest.clone(), cancellable_task())
        .await
        .unwrap();
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_observed(&api, &name, 1).await;

    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("Taskvisor cancellation did not settle")
        .unwrap();

    let cancelled = api
        .get_task(&name)
        .expect("runtime cancellation retains desired state");
    assert_eq!(cancelled.uid(), created.uid());
    assert_eq!(TaskManifest::from(&cancelled), manifest);
    assert_eq!(cancelled.status().phase(), TaskPhase::Canceled);
    assert_eq!(
        cancelled.status().reconciled().status(),
        ConditionStatus::True,
        "an accepted runtime keeps Taskvisor's authoritative outcome"
    );
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );

    let next = api
        .apply_embedded_task(
            embedded_with_revision(name.as_str(), 10_000, "test-v2"),
            cancellable_task(),
        )
        .await
        .unwrap();
    assert_eq!(next.metadata().generation(), 2);
    wait_for_observed(&api, &name, 2).await;
    api.delete_task(&name).await.unwrap();
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_runtime_reaches_exact_id_cancel_while_observed_persistence_is_saturated() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(false),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_runtime_config(runtime)
            .with_state_sink(sink.clone())
            .with_persistence_config(
                PersistenceConfig::new()
                    .try_with_state_queue_capacity(2)
                    .unwrap(),
            )
            .start()
            .await
            .unwrap(),
    );
    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "accepted-persistence-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("accepted-persistence-cancel").unwrap();
    let scheduled = api
        .write(
            embedded(name.as_str(), 10_000),
            RuntimeSource::Prebuilt(cancellable_task()),
            WriteMode::Create,
            WritePreconditions::new(),
            true,
        )
        .await
        .unwrap();
    let reconciliation = scheduled
        .reconciliation
        .expect("the created task must schedule reconciliation");
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_task(&api, &name, |task| {
        task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while api.state_persistence_status().unwrap().queued() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial target state events did not drain");

    sink.first.store(true, Ordering::Release);
    api.reconciler
        .state
        .add_task(embedded("accepted-persistence-active", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the armed persistence callback must become active");
    api.reconciler
        .state
        .add_task(embedded("accepted-persistence-buffer-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("accepted-persistence-buffer-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

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
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let accepted = api
                .reconciler
                .handle
                .list()
                .await
                .iter()
                .any(|(id, _)| *id == binding.tv);
            if accepted && api.reconciler.state.persistence_admission_waiters() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted runtime did not block only on observed-state admission");

    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if api
                .reconciler
                .handle
                .list()
                .await
                .iter()
                .all(|(id, _)| *id != binding.tv)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-accept persistence admission blocked exact-ID cancellation");
    assert!(
        !cancellation.is_finished(),
        "authoritative finalization should still honor persistence backpressure"
    );

    release_tx
        .send(())
        .expect("the test must release the active persistence callback");
    tokio::time::timeout(Duration::from_secs(5), cancellation)
        .await
        .expect("exact-ID cancellation did not settle after persistence resumed")
        .expect("cancel_task worker panicked")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reconciliation)
        .await
        .expect("reconciliation acknowledgement did not settle")
        .expect("reconciliation acknowledgement dropped");
    let cancelled = api.get_task(&name).unwrap();
    assert_eq!(cancelled.status().phase(), TaskPhase::Canceled);
    assert_eq!(cancelled.status().attempt(), 1);
    assert!(api.reconciler.state.binding_for(&name).is_none());
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_state_admission_cleans_authoritative_completion_without_synthesizing_state() {
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_state_sink(Arc::new(IgnoringStateSink))
            .start()
            .await
            .unwrap(),
    );
    let name = TaskId::new("closed-admission-authoritative-completion").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Running && task.status().attempt() == 1
    })
    .await;
    let before = api.get_task(&name).unwrap();
    let before_runs = api
        .query_task_runs(&name, &TaskRunQuery::new())
        .unwrap()
        .unwrap()
        .items;
    assert_eq!(before_runs.len(), 1);
    assert_eq!(before_runs[0].phase(), TaskPhase::Running);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if api
                .state_persistence_status()
                .is_some_and(|status| status.queued() == 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the running-state persistence event did not drain");

    api.reconciler.state.inject_persistence_worker_panic();
    api.reconciler
        .state
        .add_task(embedded("closed-admission-panic-trigger", 1_000));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = api.state_persistence_status().unwrap();
            if !status.accepting() && !status.healthy() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("injected state persistence worker panic did not close admission");

    tokio::time::timeout(Duration::from_secs(2), api.cancel_task(&name))
        .await
        .expect("closed state admission stranded authoritative cancellation")
        .unwrap();
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.subscribe_output(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );
    assert_eq!(
        api.get_task(&name).as_ref(),
        Some(&before),
        "cleanup-only fallback must not synthesize a terminal Task status"
    );
    assert_eq!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .unwrap()
            .items,
        before_runs,
        "cleanup-only fallback must not synthesize or rewrite a TaskRun"
    );

    let shutdown_api = Arc::clone(&api);
    let shutdown = tokio::spawn(async move { shutdown_api.shutdown().await });
    let shutdown = tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown hung after cleanup-only authoritative finalization");
    assert!(matches!(
        shutdown,
        Ok(Err(CoreError::ShutdownCoordinatorStopped))
    ));
}

#[tokio::test]
async fn aborted_cancel_task_after_intake_still_cancels_the_bound_taskvisor_id() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("aborted-cancel-after-intake").unwrap();
    let scheduled = api
        .write(
            embedded(name.as_str(), 10_000),
            RuntimeSource::Prebuilt(cancellable_task()),
            WriteMode::Create,
            WritePreconditions::new(),
            true,
        )
        .await
        .unwrap();
    let reconciliation = scheduled
        .reconciliation
        .expect("the created task must schedule reconciliation");
    let binding = wait_for_binding(&api, &name, 1).await;
    let observed = tokio::time::timeout(Duration::from_secs(2), reconciliation)
        .await
        .expect("runtime reconciliation did not finish")
        .expect("runtime reconciliation acknowledgement dropped");
    assert_eq!(observed.status().observed_generation(), 1);
    wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Running && task.status().attempt() == 1
    })
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.tasks.len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconciliation must hand off to the retention and completion workers");
    let (unrelated_id, unrelated_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "aborted-cancel-unrelated-taskvisor",
            cancellable_task(),
        ))
        .await
        .unwrap();

    let operation = api.task_operations.lock(&name).await;
    let tracked_before = api.reconciler.tasks.len();
    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    let registered = tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.tasks.len() <= tracked_before {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        registered.is_ok(),
        "cancel_task must register its supervisor-owned operation; tracked before = {tracked_before}, current = {}",
        api.reconciler.tasks.len()
    );

    cancellation.abort();
    assert!(
        cancellation.await.unwrap_err().is_cancelled(),
        "aborting the caller must stop only its wait"
    );
    drop(operation);

    let cancelled = wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Canceled
    })
    .await;
    assert_eq!(cancelled.status().attempt(), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.state.binding_for(&name).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Taskvisor completion must release the bound identity");
    let registered = api.reconciler.handle.list().await;
    assert!(registered.iter().all(|(id, _)| *id != binding.tv));
    assert!(
        registered.iter().any(|(id, _)| *id == unrelated_id),
        "exact-ID cancellation must not cancel an unrelated Taskvisor task"
    );
    let runs = api
        .query_task_runs(&name, &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].phase(), TaskPhase::Canceled);

    api.reconciler
        .handle
        .cancel_with_timeout(unrelated_id, Duration::from_secs(1))
        .await
        .unwrap();
    let unrelated_outcome = tokio::time::timeout(Duration::from_secs(1), unrelated_waiter.wait())
        .await
        .expect("unrelated Taskvisor task did not finish")
        .expect("unrelated Taskvisor task outcome channel closed");
    assert_eq!(unrelated_outcome.kind(), TaskOutcomeKind::Canceled);
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_task_and_apply_serialize_without_losing_the_new_generation() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_runtime_config(runtime)
            .start()
            .await
            .unwrap(),
    );
    let (held_id, held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "cancel-apply-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("cancel-apply-race").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    wait_for_task(&api, &name, |task| {
        task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;

    let operation = api.task_operations.lock(&name).await;
    let (cancel_started_tx, cancel_started_rx) = oneshot::channel();
    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancel = tokio::spawn(async move {
        let _ = cancel_started_tx.send(());
        cancel_api.cancel_task(&cancel_name).await
    });
    cancel_started_rx.await.unwrap();
    tokio::task::yield_now().await;

    let apply_api = Arc::clone(&api);
    let apply_name = name.clone();
    let apply = tokio::spawn(async move {
        apply_api
            .apply_embedded_task(
                embedded_with_revision(apply_name.as_str(), 10_000, "apply-v2"),
                cancellable_task(),
            )
            .await
    });
    tokio::task::yield_now().await;
    drop(operation);

    tokio::time::timeout(Duration::from_secs(2), cancel)
        .await
        .expect("cancel_task remained blocked behind reconciliation")
        .unwrap()
        .unwrap();
    let applied = tokio::time::timeout(Duration::from_secs(2), apply)
        .await
        .expect("apply remained blocked behind cancel_task")
        .unwrap()
        .unwrap();
    assert_eq!(applied.metadata().generation(), 2);
    let second_binding = wait_for_binding(&api, &name, 2).await;
    assert_eq!(second_binding.resource.generation, 2);

    api.reconciler
        .handle
        .cancel_with_timeout(held_id, Duration::from_secs(1))
        .await
        .unwrap();
    held_waiter.wait().await.unwrap();
    wait_for_observed(&api, &name, 2).await;
    api.delete_task(&name).await.unwrap();
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_task_and_delete_serialize_without_retaining_runtime_ownership() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("cancel-delete-race").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_observed(&api, &name, 1).await;

    let operation = api.task_operations.lock(&name).await;
    let (cancel_started_tx, cancel_started_rx) = oneshot::channel();
    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancel = tokio::spawn(async move {
        let _ = cancel_started_tx.send(());
        cancel_api.cancel_task(&cancel_name).await
    });
    cancel_started_rx.await.unwrap();
    tokio::task::yield_now().await;

    let delete_api = Arc::clone(&api);
    let delete_name = name.clone();
    let delete = tokio::spawn(async move { delete_api.delete_task(&delete_name).await });
    tokio::task::yield_now().await;
    drop(operation);

    tokio::time::timeout(Duration::from_secs(2), cancel)
        .await
        .expect("cancel_task did not settle before queued delete")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), delete)
        .await
        .expect("delete did not settle after cancel_task")
        .unwrap()
        .unwrap();
    assert!(api.get_task(&name).is_none());
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        api.reconciler.runtime_operations.lock(&name),
    )
    .await
    .expect("cancel/delete must release the runtime operation lock");
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_task_and_shutdown_settle_saturated_intake_without_a_lock_leak() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_runtime_config(runtime)
            .start()
            .await
            .unwrap(),
    );
    let (_held_id, _held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "cancel-shutdown-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("cancel-shutdown-race").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    wait_for_task(&api, &name, |task| {
        task.status().reconciled().reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;

    let start = Arc::new(tokio::sync::Barrier::new(3));
    let cancel_api = Arc::clone(&api);
    let cancel_start = Arc::clone(&start);
    let cancel_name = name.clone();
    let cancel = tokio::spawn(async move {
        cancel_start.wait().await;
        cancel_api.cancel_task(&cancel_name).await
    });
    let shutdown_api = Arc::clone(&api);
    let shutdown_start = Arc::clone(&start);
    let shutdown = tokio::spawn(async move {
        shutdown_start.wait().await;
        shutdown_api.shutdown().await
    });
    start.wait().await;

    let (cancel_result, shutdown_result) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(cancel, shutdown)
    })
    .await
    .expect("concurrent cancel and shutdown did not settle");
    match cancel_result.unwrap() {
        Ok(()) | Err(CoreError::ShuttingDown) => {}
        Err(error) => panic!("unexpected cancel/shutdown race result: {error}"),
    }
    shutdown_result.unwrap().unwrap();

    let retained = api
        .get_task(&name)
        .expect("shutdown and cancellation retain desired state");
    let reconciled = retained.status().reconciled();
    match reconciled.status() {
        ConditionStatus::False => {
            assert_eq!(reconciled.reason(), "RuntimeSubmissionCancelled");
            assert_eq!(retained.status().phase(), TaskPhase::Pending);
            assert_eq!(retained.status().attempt(), 0);
        }
        ConditionStatus::Unknown => {
            assert_eq!(reconciled.reason(), TASKVISOR_INTAKE_PENDING_REASON);
        }
        status => panic!("unexpected reconciled status after cancel/shutdown race: {status:?}"),
    }
    assert!(api.reconciler.state.binding_for(&name).is_none());
    tokio::time::timeout(
        Duration::from_secs(1),
        api.reconciler.runtime_operations.lock(&name),
    )
    .await
    .expect("cancel/shutdown must release the runtime operation lock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_an_aborted_external_cancel_operation() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("aborted-external-cancel-shutdown").unwrap();
    let scheduled = api
        .write(
            embedded(name.as_str(), 10_000),
            RuntimeSource::Prebuilt(cancellable_task()),
            WriteMode::Create,
            WritePreconditions::new(),
            true,
        )
        .await
        .unwrap();
    let reconciliation = scheduled
        .reconciliation
        .expect("the created task must schedule reconciliation");
    let binding = wait_for_binding(&api, &name, 1).await;
    let observed = tokio::time::timeout(Duration::from_secs(2), reconciliation)
        .await
        .expect("runtime reconciliation did not finish")
        .expect("runtime reconciliation acknowledgement dropped");
    assert_eq!(observed.status().observed_generation(), 1);
    wait_for_task(&api, &name, |task| {
        task.status().phase() == TaskPhase::Running && task.status().attempt() == 1
    })
    .await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.tasks.len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconciliation must hand off to the retention and completion workers");

    let operation = api.task_operations.lock(&name).await;
    let tracked_before = api.reconciler.tasks.len();
    let cancel_api = Arc::clone(&api);
    let cancel_name = name.clone();
    let cancellation = tokio::spawn(async move { cancel_api.cancel_task(&cancel_name).await });
    let registered = tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.tasks.len() <= tracked_before {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        registered.is_ok(),
        "cancel_task must register its supervisor-owned operation; tracked before = {tracked_before}, current = {}",
        api.reconciler.tasks.len()
    );
    cancellation.abort();
    assert!(cancellation.await.unwrap_err().is_cancelled());

    let shutdown_api = Arc::clone(&api);
    let shutdown = tokio::spawn(async move { shutdown_api.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !api.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown did not close operation admission");
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must drain the accepted cancel worker"
    );

    drop(operation);
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .expect("shutdown did not drain the accepted cancel worker")
        .unwrap()
        .unwrap();

    let retained = api
        .get_task(&name)
        .expect("external cancellation and shutdown retain desired state");
    assert_eq!(retained.status().phase(), TaskPhase::Canceled);
    assert_eq!(retained.status().attempt(), 1);
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );
    let runs = api
        .query_task_runs(&name, &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].phase(), TaskPhase::Canceled);
    tokio::time::timeout(Duration::from_secs(1), api.task_operations.lock(&name))
        .await
        .expect("cancel/shutdown must release the desired-state operation lock");
    tokio::time::timeout(
        Duration::from_secs(1),
        api.reconciler.runtime_operations.lock(&name),
    )
    .await
    .expect("cancel/shutdown must release the runtime operation lock");
}

#[tokio::test]
async fn shutdown_cancellation_preserves_observable_taskvisor_intake_state() {
    let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(2));
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_runtime_config(runtime)
        .start()
        .await
        .unwrap();

    let (_held_id, _held_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "shutdown-ownership-filler",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let name = TaskId::new("shutdown-ownership-intake").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    wait_for_task(&api, &name, |task| {
        let condition = task.status().reconciled();
        condition.status() == ConditionStatus::Unknown
            && condition.reason() == TASKVISOR_INTAKE_PENDING_REASON
    })
    .await;

    api.shutdown().await.unwrap();

    let retained = api
        .get_task(&name)
        .expect("desired state remains retained after shutdown");
    assert_eq!(
        retained.status().reconciled().status(),
        ConditionStatus::Unknown
    );
    assert_eq!(
        retained.status().reconciled().reason(),
        TASKVISOR_INTAKE_PENDING_REASON
    );
    assert_eq!(
        retained.status().reconciled().message(),
        TASKVISOR_INTAKE_PENDING_MESSAGE
    );
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
        guard_runtime_source(
            RuntimeSource::Prebuilt(Arc::new(DropProbeTask {
                dropped: Arc::clone(&dropped),
                gate_released: Arc::clone(&gate_released),
                api: Arc::downgrade(&api),
            })),
            desired.name().clone(),
        ),
        true,
        registration,
    );
    assert!(first_superseded.is_none());

    let admission = api
        .reconciler
        .state
        .admit_state_write(StateMutationEventCapacity::TaskChange)
        .await
        .unwrap();
    let source = guard_runtime_source(
        RuntimeSource::Prebuilt(immediate_task()),
        manifest.name().clone(),
    );
    let scheduled = api
        .write_locked(
            manifest,
            source,
            WriteMode::Apply,
            &WritePreconditions::new(),
            true,
            WriteGuards {
                operation,
                admission,
            },
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
        guard_runtime_source(
            RuntimeSource::Prebuilt(Arc::new(DropProbeTask {
                dropped: Arc::clone(&dropped),
                gate_released: Arc::new(AtomicBool::new(false)),
                api: Weak::new(),
            })),
            desired.name().clone(),
        ),
        true,
        api.reconciler.tasks.token(),
    );
    assert!(first_superseded.is_none());

    let (_second_completion, superseded) = api.reconciler.schedule(
        desired.clone(),
        guard_runtime_source(
            RuntimeSource::Prebuilt(immediate_task()),
            desired.name().clone(),
        ),
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
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(Arc::new(IgnoringStateSink))
        .start()
        .await
        .unwrap();
    let retained = embedded("apply-after-close", 1_000);
    api.reconciler.state.add_task(retained.clone());
    let before = api
        .get_task(&TaskId::new("apply-after-close").unwrap())
        .unwrap();
    api.shutdown().await.unwrap();

    let error = api
        .create_embedded_task(embedded("too-late", 1_000), immediate_task())
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown));
    assert!(api.get_task(&TaskId::new("too-late").unwrap()).is_none());

    let error = api
        .apply_task_where(retained, WritePreconditions::new(), |_| true)
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown));
    assert_eq!(
        api.get_task(&TaskId::new("apply-after-close").unwrap()),
        Some(before)
    );
}

#[tokio::test]
async fn delete_after_shutdown_is_rejected_without_mutation_or_state_sink() {
    let api = api(RunnerRouter::new()).await;
    let name = TaskId::new("delete-after-close-no-sink").unwrap();
    api.reconciler
        .state
        .add_task(embedded(name.as_str(), 1_000));
    let before = api.get_task(&name).expect("the retained task must exist");
    api.shutdown().await.unwrap();

    let error = api.delete_task(&name).await.unwrap_err();

    assert!(matches!(error, CoreError::ShuttingDown));
    assert_eq!(api.get_task(&name), Some(before));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_deletes_bypass_owned_workers_and_saturated_persistence_admission() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(true),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_state_sink(sink)
            .with_persistence_config(
                PersistenceConfig::new()
                    .try_with_state_queue_capacity(2)
                    .unwrap(),
            )
            .start()
            .await
            .unwrap(),
    );

    api.reconciler
        .state
        .add_task(embedded("missing-delete-persistence-active", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the persistence callback must become active");
    api.reconciler
        .state
        .add_task(embedded("missing-delete-buffer-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("missing-delete-buffer-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

    let mut deletions = Vec::new();
    for index in 0..64 {
        let delete_api = Arc::clone(&api);
        let name = TaskId::new(format!("missing-delete-{index}")).unwrap();
        deletions.push(tokio::spawn(
            async move { delete_api.delete_task(&name).await },
        ));
    }
    let completed_while_saturated = tokio::time::timeout(Duration::from_secs(2), async {
        while deletions.iter().any(|deletion| !deletion.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let tracked_while_saturated = api.delete_operations.len();
    let admission_waiters_while_saturated = api.reconciler.state.persistence_admission_waiters();
    let queued_while_saturated = api.state_persistence_status().unwrap().queued();

    release_tx
        .send(())
        .expect("the persistence callback must remain blocked until observation");
    for deletion in deletions {
        deletion.await.unwrap().unwrap();
    }
    api.shutdown().await.unwrap();

    assert!(
        completed_while_saturated,
        "missing deletes must not wait for saturated persistence"
    );
    assert_eq!(
        tracked_while_saturated, 0,
        "missing deletes must not register SDK-owned workers"
    );
    assert_eq!(
        admission_waiters_while_saturated, 0,
        "missing deletes must not acquire persistence admission"
    );
    assert_eq!(
        queued_while_saturated, 3,
        "missing deletes must not mutate the saturated persistence queue"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_delete_during_scheduled_settlement_still_removes_desired_state() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("aborted-delete-scheduled-settlement").unwrap();
    let (entered, release) = api
        .reconciler
        .arm_after_provisional_bind_panic(name.clone());

    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("reconciliation did not reach provisional binding")
        .expect("provisional binding hook was dropped");
    let provisional = api
        .reconciler
        .state
        .binding_for(&name)
        .expect("the paused reconciliation must retain its provisional binding");
    let deletion = spawn_registered_delete(&api, &name).await;
    assert!(
        !deletion.is_finished(),
        "delete must wait for scheduled reconciliation settlement"
    );

    deletion.abort();
    assert!(deletion.await.unwrap_err().is_cancelled());
    assert!(api.get_task(&name).is_some());
    let shutdown_api = Arc::clone(&api);
    let shutdown = tokio::spawn(async move { shutdown_api.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !api.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown did not close delete admission");
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for scheduled delete settlement"
    );
    release
        .send(())
        .expect("the paused reconciliation must remain owned by its coordinator");

    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown did not drain the settled delete worker")
        .unwrap()
        .unwrap();
    assert!(api.get_task(&name).is_none());
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(api.subscribe_output(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != provisional.tv),
        "an unsubmitted provisional identity must not enter Taskvisor"
    );
    assert!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .is_none(),
        "delete must not synthesize a TaskRun for unsubmitted work"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_delete_before_runtime_lock_still_cancels_only_the_bound_taskvisor_id() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("aborted-delete-exact-runtime").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_observed(&api, &name, 1).await;
    let (unrelated_id, unrelated_waiter) = api
        .reconciler
        .handle
        .add_and_watch(TvTaskSpec::once(
            "aborted-delete-unrelated-taskvisor",
            cancellable_task(),
        ))
        .await
        .unwrap();
    let runtime_operation = api.reconciler.runtime_operations.lock(&name).await;
    let deletion = spawn_registered_delete(&api, &name).await;
    deletion.abort();
    assert!(deletion.await.unwrap_err().is_cancelled());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .any(|(id, _)| *id == binding.tv),
        "the runtime lock must stage exact cancellation"
    );

    drop(runtime_operation);
    wait_for_deleted(&api, &name).await;
    assert!(api.reconciler.state.binding_for(&name).is_none());
    let registered = api.reconciler.handle.list().await;
    assert!(registered.iter().all(|(id, _)| *id != binding.tv));
    assert!(
        registered.iter().any(|(id, _)| *id == unrelated_id),
        "exact-ID cancellation must preserve unrelated Taskvisor work"
    );
    assert!(
        api.query_task_runs(&name, &TaskRunQuery::new())
            .unwrap()
            .is_none(),
        "local deletion must remove the authoritative run history"
    );

    api.reconciler
        .handle
        .cancel_with_timeout(unrelated_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        unrelated_waiter.wait().await.unwrap().kind(),
        TaskOutcomeKind::Canceled
    );
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_delete_at_state_admission_finishes_and_repeated_delete_is_idempotent() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let sink = Arc::new(TokioDependentStateSink {
        first: AtomicBool::new(false),
        events: AtomicUsize::new(0),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let api = Arc::new(
        SupervisorApi::builder(RunnerRouter::new())
            .with_state_sink(sink.clone())
            .with_persistence_config(
                PersistenceConfig::new()
                    .try_with_state_queue_capacity(2)
                    .unwrap(),
            )
            .start()
            .await
            .unwrap(),
    );
    let name = TaskId::new("aborted-delete-state-admission").unwrap();
    api.reconciler
        .state
        .add_task(embedded(name.as_str(), 1_000));
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.state_persistence_status().unwrap().queued() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the target creation event did not drain");

    sink.first.store(true, Ordering::Release);
    api.reconciler
        .state
        .add_task(embedded("delete-state-admission-active", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the persistence callback must become active");
    api.reconciler
        .state
        .add_task(embedded("delete-state-admission-buffer-one", 1_000));
    api.reconciler
        .state
        .add_task(embedded("delete-state-admission-buffer-two", 1_000));
    assert_eq!(api.state_persistence_status().unwrap().queued(), 3);

    let deletion = spawn_registered_delete(&api, &name).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while api.reconciler.state.persistence_admission_waiters() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete did not reach final state mutation admission");

    deletion.abort();
    assert!(deletion.await.unwrap_err().is_cancelled());
    assert!(api.get_task(&name).is_some());
    release_tx
        .send(())
        .expect("the persistence callback must still own the staged delete");
    wait_for_deleted(&api, &name).await;

    api.delete_task(&name).await.unwrap();
    assert!(api.get_task(&name).is_none());
    api.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_an_aborted_delete_worker() {
    let api = Arc::new(api(RunnerRouter::new()).await);
    let name = TaskId::new("aborted-delete-shutdown-drain").unwrap();
    api.create_embedded_task(embedded(name.as_str(), 10_000), cancellable_task())
        .await
        .unwrap();
    let binding = wait_for_binding(&api, &name, 1).await;
    wait_for_observed(&api, &name, 1).await;
    let runtime_operation = api.reconciler.runtime_operations.lock(&name).await;
    let deletion = spawn_registered_delete(&api, &name).await;
    deletion.abort();
    assert!(deletion.await.unwrap_err().is_cancelled());

    let shutdown_api = Arc::clone(&api);
    let shutdown = tokio::spawn(async move { shutdown_api.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !api.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown did not close operation admission");
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for the accepted delete worker"
    );

    drop(runtime_operation);
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown did not drain the accepted delete worker")
        .unwrap()
        .unwrap();
    api.shutdown().await.unwrap();
    assert!(api.get_task(&name).is_none());
    assert!(api.reconciler.state.binding_for(&name).is_none());
    assert!(
        api.reconciler
            .handle
            .list()
            .await
            .iter()
            .all(|(id, _)| *id != binding.tv)
    );
    tokio::time::timeout(Duration::from_secs(1), api.task_operations.lock(&name))
        .await
        .expect("delete/shutdown must release the desired-state operation lock");
    tokio::time::timeout(
        Duration::from_secs(1),
        api.reconciler.runtime_operations.lock(&name),
    )
    .await
    .expect("delete/shutdown must release the runtime operation lock");
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
