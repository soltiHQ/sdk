use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::Duration,
};

use async_trait::async_trait;
use solti_model::{ContainerSpec, TaskEnv, TaskSpec};
use taskvisor::{
    RuntimeError, Supervisor, SupervisorConfig, TaskOutcomeKind, TaskSpec as TaskvisorTaskSpec,
};
use tokio::sync::{Notify, Semaphore};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    instrument::WithSubscriber as _,
    span::{Attributes, Id, Record},
};

use super::*;
use crate::container::{ContainerEngineInfo, ContainerOutput};

#[derive(Default)]
struct TraceCapture {
    fields: Mutex<Vec<String>>,
}

struct CaptureSubscriber(Arc<TraceCapture>);

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        event.record(&mut CaptureVisitor(&self.0));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

struct CaptureVisitor<'a>(&'a TraceCapture);

impl Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .fields
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Probe,
    Create { attempt: u32, image: String },
    Start,
    Wait,
    Terminate,
    Cleanup,
    AttemptDropped,
    Shutdown,
}

struct FakeEngine {
    calls: Arc<Mutex<Vec<Call>>>,
    exit_code: i32,
    create_error: Option<ContainerErrorClass>,
}

impl FakeEngine {
    fn new(exit_code: i32) -> (Arc<Self>, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
                exit_code,
                create_error: None,
            }),
            calls,
        )
    }

    fn failing(class: ContainerErrorClass) -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            exit_code: 0,
            create_error: Some(class),
        })
    }
}

#[async_trait]
impl ContainerEngine for FakeEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Probe);
        Ok(ContainerEngineInfo::new("fake", "1"))
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Create {
            attempt: request.attempt(),
            image: request.image().to_owned(),
        });
        if let Some(class) = self.create_error {
            return Err(match class {
                ContainerErrorClass::Retryable => {
                    ContainerEngineError::retryable("temporary create failure")
                }
                ContainerErrorClass::Permanent => {
                    ContainerEngineError::permanent("permanent create failure")
                }
            });
        }
        Ok(Box::new(FakeAttempt {
            calls: Arc::clone(&self.calls),
            exit_code: self.exit_code,
        }))
    }
}

#[async_trait]
impl super::super::ContainerEngineFinalizer for FakeEngine {
    async fn shutdown(&self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Shutdown);
        Ok(())
    }
}

struct FakeAttempt {
    calls: Arc<Mutex<Vec<Call>>>,
    exit_code: i32,
}

#[async_trait]
impl ContainerAttempt for FakeAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        None
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        None
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Start);
        Ok(())
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Wait);
        Ok(ContainerExitStatus::new(self.exit_code))
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Terminate);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Cleanup);
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct AttemptBehavior {
    start_fails: bool,
    terminate_fails: bool,
    cleanup_fails: bool,
}

struct ControlledEngine {
    calls: Arc<Mutex<Vec<Call>>>,
    create_entered: Arc<Notify>,
    create_release: Arc<Semaphore>,
    create_in_flight: Arc<AtomicBool>,
    create_rollback_error: bool,
    behavior: AttemptBehavior,
}

struct ControlledEngineHandle {
    calls: Arc<Mutex<Vec<Call>>>,
    create_entered: Arc<Notify>,
    create_release: Arc<Semaphore>,
    create_in_flight: Arc<AtomicBool>,
}

struct InFlightCreate(Arc<AtomicBool>);

impl InFlightCreate {
    fn enter(flag: Arc<AtomicBool>) -> Self {
        assert!(
            !flag.swap(true, Ordering::SeqCst),
            "controlled create attempts must not overlap"
        );
        Self(flag)
    }
}

impl Drop for InFlightCreate {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl ControlledEngine {
    fn blocked(behavior: AttemptBehavior) -> (Arc<Self>, ControlledEngineHandle) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let create_entered = Arc::new(Notify::new());
        let create_release = Arc::new(Semaphore::new(0));
        let create_in_flight = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
                create_entered: Arc::clone(&create_entered),
                create_release: Arc::clone(&create_release),
                create_in_flight: Arc::clone(&create_in_flight),
                create_rollback_error: false,
                behavior,
            }),
            ControlledEngineHandle {
                calls,
                create_entered,
                create_release,
                create_in_flight,
            },
        )
    }

    fn blocked_with_rollback_error() -> (Arc<Self>, ControlledEngineHandle) {
        let (mut engine, handle) = Self::blocked(AttemptBehavior::default());
        Arc::get_mut(&mut engine)
            .expect("new controlled engine must be uniquely owned")
            .create_rollback_error = true;
        (engine, handle)
    }
}

#[async_trait]
impl ContainerEngine for ControlledEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Probe);
        Ok(ContainerEngineInfo::new("controlled", "1"))
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Create {
            attempt: request.attempt(),
            image: request.image().to_owned(),
        });
        let _in_flight = InFlightCreate::enter(Arc::clone(&self.create_in_flight));
        self.create_entered.notify_one();
        let permit = self
            .create_release
            .acquire()
            .await
            .map_err(|_| ContainerEngineError::permanent("test create gate closed"))?;
        permit.forget();
        if self.create_rollback_error {
            return Err(ContainerEngineError::permanent(
                "containerd attempt creation failed and rollback was incomplete",
            ));
        }
        Ok(Box::new(ControlledAttempt {
            calls: Arc::clone(&self.calls),
            behavior: self.behavior,
        }))
    }
}

struct ControlledAttempt {
    calls: Arc<Mutex<Vec<Call>>>,
    behavior: AttemptBehavior,
}

#[async_trait]
impl ContainerAttempt for ControlledAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        None
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        None
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Start);
        if self.behavior.start_fails {
            Err(ContainerEngineError::retryable("controlled start failure"))
        } else {
            Ok(())
        }
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Wait);
        Ok(ContainerExitStatus::new(0))
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Terminate);
        if self.behavior.terminate_fails {
            Err(ContainerEngineError::retryable(
                "controlled termination failure",
            ))
        } else {
            Ok(())
        }
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Cleanup);
        if self.behavior.cleanup_fails {
            Err(ContainerEngineError::retryable(
                "controlled cleanup failure",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockingStage {
    Start,
    Wait,
    Terminate,
    Cleanup,
}

struct StageDropState {
    target: BlockingStage,
    calls: Arc<Mutex<Vec<Call>>>,
    entered: Notify,
    wait_entered: Notify,
    in_flight: Mutex<Option<BlockingStage>>,
    attempt_dropped: AtomicBool,
}

impl StageDropState {
    async fn block(self: &Arc<Self>, stage: BlockingStage) {
        let _in_flight = StageInFlight::enter(Arc::clone(self), stage);
        if stage == self.target {
            self.entered.notify_one();
        }
        std::future::pending::<()>().await;
    }
}

struct StageInFlight {
    state: Arc<StageDropState>,
    stage: BlockingStage,
}

impl StageInFlight {
    fn enter(state: Arc<StageDropState>, stage: BlockingStage) -> Self {
        let previous = state.in_flight.lock().unwrap().replace(stage);
        assert!(previous.is_none(), "attempt stages must not overlap");
        Self { state, stage }
    }
}

impl Drop for StageInFlight {
    fn drop(&mut self) {
        let previous = self.state.in_flight.lock().unwrap().take();
        assert_eq!(previous, Some(self.stage));
    }
}

struct StageDropEngine {
    state: Arc<StageDropState>,
}

struct StageDropHandle {
    state: Arc<StageDropState>,
}

impl StageDropEngine {
    fn blocking(target: BlockingStage) -> (Arc<Self>, StageDropHandle) {
        let state = Arc::new(StageDropState {
            target,
            calls: Arc::new(Mutex::new(Vec::new())),
            entered: Notify::new(),
            wait_entered: Notify::new(),
            in_flight: Mutex::new(None),
            attempt_dropped: AtomicBool::new(false),
        });
        (
            Arc::new(Self {
                state: Arc::clone(&state),
            }),
            StageDropHandle { state },
        )
    }
}

#[async_trait]
impl ContainerEngine for StageDropEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        Ok(ContainerEngineInfo::new("stage-drop", "1"))
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.state.calls.lock().unwrap().push(Call::Create {
            attempt: request.attempt(),
            image: request.image().to_owned(),
        });
        Ok(Box::new(StageDropAttempt {
            state: Arc::clone(&self.state),
        }))
    }
}

struct StageDropAttempt {
    state: Arc<StageDropState>,
}

impl Drop for StageDropAttempt {
    fn drop(&mut self) {
        self.state.attempt_dropped.store(true, Ordering::SeqCst);
        self.state.calls.lock().unwrap().push(Call::AttemptDropped);
    }
}

#[async_trait]
impl ContainerAttempt for StageDropAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        None
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        None
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.state.calls.lock().unwrap().push(Call::Start);
        if self.state.target == BlockingStage::Start {
            self.state.block(BlockingStage::Start).await;
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.state.calls.lock().unwrap().push(Call::Wait);
        self.state.wait_entered.notify_one();
        if matches!(
            self.state.target,
            BlockingStage::Wait | BlockingStage::Terminate
        ) {
            self.state.block(BlockingStage::Wait).await;
        }
        Ok(ContainerExitStatus::new(0))
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.state.calls.lock().unwrap().push(Call::Terminate);
        if self.state.target == BlockingStage::Terminate {
            self.state.block(BlockingStage::Terminate).await;
        }
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.state.calls.lock().unwrap().push(Call::Cleanup);
        if self.state.target == BlockingStage::Cleanup {
            self.state.block(BlockingStage::Cleanup).await;
        }
        Ok(())
    }
}

struct CooperativeCancelEngine {
    calls: Arc<Mutex<Vec<Call>>>,
    wait_entered: Arc<Notify>,
    wait_calls: Arc<AtomicUsize>,
    dropped_after_cleanup: Arc<AtomicBool>,
}

struct CooperativeCancelHandle {
    calls: Arc<Mutex<Vec<Call>>>,
    wait_entered: Arc<Notify>,
    dropped_after_cleanup: Arc<AtomicBool>,
}

impl CooperativeCancelEngine {
    fn new() -> (Arc<Self>, CooperativeCancelHandle) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let wait_entered = Arc::new(Notify::new());
        let wait_calls = Arc::new(AtomicUsize::new(0));
        let dropped_after_cleanup = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
                wait_entered: Arc::clone(&wait_entered),
                wait_calls,
                dropped_after_cleanup: Arc::clone(&dropped_after_cleanup),
            }),
            CooperativeCancelHandle {
                calls,
                wait_entered,
                dropped_after_cleanup,
            },
        )
    }
}

#[async_trait]
impl ContainerEngine for CooperativeCancelEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        Ok(ContainerEngineInfo::new("cooperative-cancel", "1"))
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Create {
            attempt: request.attempt(),
            image: request.image().to_owned(),
        });
        Ok(Box::new(CooperativeCancelAttempt {
            calls: Arc::clone(&self.calls),
            wait_entered: Arc::clone(&self.wait_entered),
            wait_calls: Arc::clone(&self.wait_calls),
            dropped_after_cleanup: Arc::clone(&self.dropped_after_cleanup),
            cleanup_complete: false,
        }))
    }
}

struct CooperativeCancelAttempt {
    calls: Arc<Mutex<Vec<Call>>>,
    wait_entered: Arc<Notify>,
    wait_calls: Arc<AtomicUsize>,
    dropped_after_cleanup: Arc<AtomicBool>,
    cleanup_complete: bool,
}

impl Drop for CooperativeCancelAttempt {
    fn drop(&mut self) {
        self.dropped_after_cleanup
            .store(self.cleanup_complete, Ordering::SeqCst);
        self.calls.lock().unwrap().push(Call::AttemptDropped);
    }
}

#[async_trait]
impl ContainerAttempt for CooperativeCancelAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        None
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        None
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Start);
        Ok(())
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Wait);
        if self.wait_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.wait_entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ContainerExitStatus::new(0))
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Terminate);
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.calls.lock().unwrap().push(Call::Cleanup);
        self.cleanup_complete = true;
        Ok(())
    }
}

async fn wait_for(signal: &Notify, operation: &str) {
    tokio::time::timeout(Duration::from_secs(1), signal.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {operation}"));
}

fn worker_context(engine: Arc<dyn ContainerEngine>) -> Arc<TaskExecContext> {
    let resource = task();
    let run_id = solti_runner::make_run_id("containerd", resource.slot().as_str());
    let build = BuildContext::default();
    let TaskWorkload::Container(spec) = resource.spec().workload() else {
        unreachable!("test task is a container workload");
    };
    Arc::new(TaskExecContext {
        engine,
        metrics: build.metrics().clone(),
        output_publisher: Arc::clone(build.output_publisher()),
        task: ContainerTaskConfig {
            run_id: Arc::from(run_id.name()),
            resource_name: resource.name().clone(),
            generation: resource.metadata().generation(),
            image: spec.image.clone(),
            command: spec.command.clone(),
            args: spec.args.clone(),
            env: std::collections::BTreeMap::new(),
            process_policy: super::super::ContainerProcessPolicy::default(),
        },
        logger: LogConfig::default(),
        attempt: AtomicU32::new(0),
    })
}

fn task_with_image(image: impl Into<String>) -> Task {
    let workload = TaskWorkload::Container(ContainerSpec::new(
        image.into(),
        Some(vec!["echo".into()]),
        vec!["hello".into()],
        TaskEnv::new(),
    ));
    let spec = TaskSpec::builder("container-slot", workload, 5_000_u64)
        .build()
        .unwrap();
    Task::new("container-task", spec).unwrap()
}

fn task() -> Task {
    task_with_image("docker.io/library/alpine:latest")
}

async fn build(runner: &ContainerRunner) -> TaskRef {
    let resource = task();
    let run_id = solti_runner::make_run_id(runner.name(), resource.slot().as_str());
    let mut scope = solti_runner::BuildScope::unmanaged(runner.name());
    runner
        .build_task(
            &resource,
            &run_id,
            &BuildContext::default(),
            &solti_runner::BuildCancellation::new(),
            &mut scope,
        )
        .await
        .unwrap()
}

fn drop_releasing_engine<E>(engine: Arc<E>) -> ContainerEngineBinding
where
    E: ContainerEngine,
{
    let engine: Arc<dyn ContainerEngine> = engine;
    ContainerEngineBinding::drop_releases(engine)
}

#[test]
fn engine_binding_records_explicit_contract_and_redacts_engine() {
    let (engine, _) = FakeEngine::new(0);
    let engine: Arc<dyn ContainerEngine> = engine;
    let drop_releases = ContainerEngineBinding::drop_releases(Arc::clone(&engine));
    assert_eq!(
        drop_releases.ownership_contract(),
        ContainerOwnershipContract::DropReleases,
    );
    let debug = format!("{drop_releases:?}");
    assert!(debug.contains("DropReleases"), "{debug}");
    assert!(debug.contains("<engine>"), "{debug}");

    let finalizer = ContainerEngineBinding::pre_admitted_finalizer(engine);
    assert_eq!(
        finalizer.ownership_contract(),
        ContainerOwnershipContract::PreAdmittedFinalizer,
    );

    let runner = ContainerRunner::new("redacted", finalizer).unwrap();
    let debug = format!("{runner:?}");
    assert!(debug.contains("PreAdmittedFinalizer"), "{debug}");
    assert!(debug.contains("<engine>"), "{debug}");
}

#[tokio::test]
async fn typed_finalizer_binding_returns_awaitable_shutdown_capability() {
    let (engine, calls) = FakeEngine::new(0);
    let (binding, shutdown) = ContainerEngineBinding::pre_admitted_finalizer_with_shutdown(engine);

    assert_eq!(
        binding.ownership_contract(),
        ContainerOwnershipContract::PreAdmittedFinalizer,
    );
    shutdown.shutdown().await.unwrap();
    assert_eq!(*calls.lock().unwrap(), [Call::Shutdown]);
    assert!(format!("{shutdown:?}").contains("<engine>"));
}

#[tokio::test]
async fn build_has_no_engine_io_and_each_spawn_gets_one_attempt() {
    let (engine, calls) = FakeEngine::new(0);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();
    let task = build(&runner).await;

    assert!(calls.lock().unwrap().is_empty());
    task.spawn(TaskContext::detached()).await.unwrap();
    task.spawn(TaskContext::detached()).await.unwrap();

    assert_eq!(
        *calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Wait,
            Call::Cleanup,
            Call::Create {
                attempt: 2,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Wait,
            Call::Cleanup,
        ]
    );
}

#[tokio::test]
async fn tracing_does_not_record_container_image() {
    const SECRET: &str = "container-credential-secret";
    const FORGED: &str = "forged-container-record";

    let (engine, _) = FakeEngine::new(0);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();
    let capture = Arc::new(TraceCapture::default());
    let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
    let _interest_guard = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));

    build(&runner)
        .with_subscriber(dispatch.clone())
        .await
        .spawn(TaskContext::detached())
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    tracing::dispatcher::with_default(&dispatch, tracing::callsite::rebuild_interest_cache);
    capture.fields.lock().unwrap().clear();

    let resource = task_with_image(format!(
        "https://user:{SECRET}@registry.invalid/private\n{FORGED}"
    ));
    let run_id = solti_runner::make_run_id(runner.name(), resource.slot().as_str());
    let mut scope = solti_runner::BuildScope::unmanaged(runner.name());
    let task = runner
        .build_task(
            &resource,
            &run_id,
            &BuildContext::default(),
            &solti_runner::BuildCancellation::new(),
            &mut scope,
        )
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    task.spawn(TaskContext::detached())
        .with_subscriber(dispatch)
        .await
        .unwrap();

    let fields = capture.fields.lock().unwrap().join(" ");
    assert!(fields.contains("container.build"), "{fields}");
    assert!(fields.contains("container.lifecycle"), "{fields}");
    assert!(fields.contains("creating"), "{fields}");
    assert!(!fields.contains("image="), "{fields}");
    assert!(!fields.contains(SECRET), "{fields}");
    assert!(!fields.contains(FORGED), "{fields}");
}

#[tokio::test]
async fn pre_canceled_attempt_performs_no_engine_io() {
    let (engine, calls) = FakeEngine::new(0);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();

    let error = build(&runner)
        .await
        .spawn(TaskContext::detached_cancelled())
        .await
        .unwrap_err();

    assert!(matches!(error, TaskError::Canceled));
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn engine_class_and_exit_code_reach_taskvisor() {
    let retryable = ContainerRunner::new(
        "retryable",
        drop_releasing_engine(FakeEngine::failing(ContainerErrorClass::Retryable)),
    )
    .unwrap();
    assert!(matches!(
        build(&retryable).await.spawn(TaskContext::detached()).await,
        Err(TaskError::Fail { .. })
    ));

    let permanent = ContainerRunner::new(
        "permanent",
        drop_releasing_engine(FakeEngine::failing(ContainerErrorClass::Permanent)),
    )
    .unwrap();
    assert!(matches!(
        build(&permanent).await.spawn(TaskContext::detached()).await,
        Err(TaskError::Fatal { .. })
    ));

    let (engine, _) = FakeEngine::new(23);
    let failed = ContainerRunner::new("exit-code", drop_releasing_engine(engine)).unwrap();
    assert!(matches!(
        build(&failed).await.spawn(TaskContext::detached()).await,
        Err(TaskError::Fail {
            exit_code: Some(23),
            ..
        })
    ));
}

#[tokio::test]
async fn cancellation_during_create_cleans_without_starting() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
    let ctx = worker_context(engine);
    let request = ctx.task.request(1);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let worker = tokio::spawn(run_container_worker(ctx, request, None, cancel_rx));

    wait_for(&handle.create_entered, "create attempt to start").await;
    cancel_tx.send(true).unwrap();
    handle.create_release.add_permits(1);

    let result = tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("container worker did not finish")
        .expect("container worker panicked");
    assert!(matches!(result, Err(TaskError::Canceled)));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Cleanup,
        ]
    );
}

#[tokio::test]
async fn create_rollback_error_wins_over_concurrent_cancellation() {
    let (engine, handle) = ControlledEngine::blocked_with_rollback_error();
    let ctx = worker_context(engine);
    let request = ctx.task.request(1);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let worker = tokio::spawn(run_container_worker(ctx, request, None, cancel_rx));

    wait_for(&handle.create_entered, "create attempt to start").await;
    cancel_tx.send(true).unwrap();
    handle.create_release.add_permits(1);

    let error = tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("container worker did not finish")
        .expect("container worker panicked")
        .expect_err("rollback failure must fail the attempt");
    assert!(matches!(error, TaskError::Fatal { .. }), "{error}");
    assert!(
        error
            .to_string()
            .contains("containerd attempt creation failed and rollback was incomplete"),
        "{error}"
    );
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [Call::Create {
            attempt: 1,
            image: "docker.io/library/alpine:latest".into(),
        }]
    );
}

#[tokio::test]
async fn cooperative_cancellation_waits_for_termination_wait_and_cleanup() {
    let (engine, handle) = CooperativeCancelEngine::new();
    let ctx = worker_context(engine);
    let request = ctx.task.request(1);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let worker = tokio::spawn(run_container_worker(ctx, request, None, cancel_rx));

    wait_for(&handle.wait_entered, "container wait to start").await;
    cancel_tx.send(true).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("container worker did not finish")
        .expect("container worker panicked");
    assert!(matches!(result, Err(TaskError::Canceled)));
    assert!(handle.dropped_after_cleanup.load(Ordering::SeqCst));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Wait,
            Call::Terminate,
            Call::Wait,
            Call::Cleanup,
            Call::AttemptDropped,
        ]
    );
}

async fn force_drop_at_attempt_stage(target: BlockingStage) {
    let (engine, handle) = StageDropEngine::blocking(target);
    let ctx = worker_context(engine);
    let request = ctx.task.request(1);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let worker = tokio::spawn(run_container_worker(ctx, request, None, cancel_rx));

    if target == BlockingStage::Terminate {
        wait_for(&handle.state.wait_entered, "container wait to start").await;
        cancel_tx.send(true).unwrap();
    }
    wait_for(&handle.state.entered, "selected container stage to start").await;

    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(handle.state.attempt_dropped.load(Ordering::SeqCst));
    assert!(handle.state.in_flight.lock().unwrap().is_none());

    let calls = handle.state.calls.lock().unwrap().clone();
    assert_eq!(calls.last(), Some(&Call::AttemptDropped));
    let stage_call = match target {
        BlockingStage::Start => Call::Start,
        BlockingStage::Wait => Call::Wait,
        BlockingStage::Terminate => Call::Terminate,
        BlockingStage::Cleanup => Call::Cleanup,
    };
    assert!(calls.contains(&stage_call), "calls: {calls:?}");

    tokio::task::yield_now().await;
    assert_eq!(*handle.state.calls.lock().unwrap(), calls);
}

#[tokio::test]
async fn force_drop_drops_every_in_flight_attempt_stage_without_runner_detach() {
    for stage in [
        BlockingStage::Start,
        BlockingStage::Wait,
        BlockingStage::Terminate,
        BlockingStage::Cleanup,
    ] {
        force_drop_at_attempt_stage(stage).await;
    }
}

#[tokio::test]
async fn dropping_outer_future_drops_in_flight_create() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();
    let task = build(&runner).await;
    let outer = tokio::spawn(async move { task.spawn(TaskContext::detached()).await });

    wait_for(&handle.create_entered, "create attempt to start").await;
    assert!(handle.create_in_flight.load(Ordering::SeqCst));
    outer.abort();
    assert!(outer.await.unwrap_err().is_cancelled());
    assert!(!handle.create_in_flight.load(Ordering::SeqCst));

    handle.create_release.add_permits(1);
    tokio::task::yield_now().await;
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [Call::Create {
            attempt: 1,
            image: "docker.io/library/alpine:latest".into(),
        }]
    );
}

#[tokio::test]
async fn taskvisor_force_abort_reports_outcome_and_releases_yielding_create() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();
    let supervisor = Supervisor::new(SupervisorConfig::new().with_grace(Duration::ZERO), vec![]);
    let supervisor_handle = supervisor.serve().unwrap();
    let (_, waiter) = supervisor_handle
        .add_and_watch(TaskvisorTaskSpec::once(
            "container-force-abort",
            build(&runner).await,
        ))
        .await
        .unwrap();

    wait_for(&handle.create_entered, "create attempt to start").await;
    assert!(handle.create_in_flight.load(Ordering::SeqCst));

    let shutdown = supervisor_handle.shutdown().await;
    assert!(matches!(shutdown, Err(RuntimeError::GraceExceeded { .. })));
    let outcome = waiter.wait().await.unwrap();
    assert_eq!(outcome.kind(), TaskOutcomeKind::ForceAborted);

    // ForceAborted is a bounded logical outcome in Taskvisor 0.8. Physical
    // release can happen later when task code is synchronously blocking. This
    // engine future yields to Tokio and must still be released without detach.
    tokio::time::timeout(Duration::from_secs(1), async {
        while handle.create_in_flight.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("yielding create future remained physically active after force-abort");
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [Call::Create {
            attempt: 1,
            image: "docker.io/library/alpine:latest".into(),
        }]
    );
}

#[tokio::test]
async fn taskvisor_timeout_drops_lifecycle_before_outcome() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();
    let supervisor = Supervisor::new(SupervisorConfig::new(), vec![]);
    let supervisor_handle = supervisor.serve().unwrap();
    let (_, waiter) = supervisor_handle
        .add_and_watch(
            TaskvisorTaskSpec::once("container-timeout", build(&runner).await)
                .with_timeout(Duration::from_millis(20)),
        )
        .await
        .unwrap();

    wait_for(&handle.create_entered, "create attempt to start").await;
    let outcome = tokio::time::timeout(Duration::from_secs(1), waiter.wait())
        .await
        .expect("timed out waiting for Taskvisor outcome")
        .unwrap();
    assert_eq!(outcome.kind(), TaskOutcomeKind::Failed);
    assert!(!handle.create_in_flight.load(Ordering::SeqCst));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [Call::Create {
            attempt: 1,
            image: "docker.io/library/alpine:latest".into(),
        }]
    );
    supervisor_handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_output_tasks_aborts_readers() {
    let stdout = tokio::spawn(std::future::pending::<()>());
    let stderr = tokio::spawn(std::future::pending::<()>());
    let stdout_abort = stdout.abort_handle();
    let stderr_abort = stderr.abort_handle();
    tokio::task::yield_now().await;

    drop(OutputTasks {
        stdout: Some(stdout),
        stderr: Some(stderr),
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        while !stdout_abort.is_finished() || !stderr_abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("output tasks were not aborted on drop");
}

struct PanicAfterStdoutAttempt {
    stdout: Option<ContainerOutput>,
}

#[async_trait]
impl ContainerAttempt for PanicAfterStdoutAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        panic!("hostile attempt panicked while taking stderr")
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        unreachable!("hostile attempt must panic before start")
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        unreachable!("hostile attempt must panic before wait")
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        unreachable!("hostile attempt must panic before terminate")
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        unreachable!("hostile attempt must panic before cleanup")
    }
}

#[tokio::test]
async fn panic_while_taking_stderr_aborts_the_owned_stdout_reader() {
    use tokio::io::AsyncReadExt as _;

    let (stdout_reader, mut stdout_peer) = tokio::io::duplex(64);
    let outer = tokio::spawn(async move {
        let mut attempt = PanicAfterStdoutAttempt {
            stdout: Some(Box::pin(stdout_reader)),
        };
        let _output = OutputTasks::start(
            &mut attempt,
            Arc::from("panic-between-output-readers"),
            LogConfig::default(),
            None,
        );
    });

    assert!(
        outer
            .await
            .expect_err("hostile stderr accessor must panic")
            .is_panic()
    );

    let mut probe = [0_u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stdout_peer.read(&mut probe))
            .await
            .expect("stdout reader task remained detached after owner unwind")
            .expect("stdout peer read must succeed"),
        0,
        "stdout peer EOF proves the spawned reader released its endpoint",
    );
}

#[tokio::test]
async fn start_failure_terminates_and_cleans_attempt() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior {
        start_fails: true,
        terminate_fails: false,
        cleanup_fails: false,
    });
    handle.create_release.add_permits(1);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();

    let result = build(&runner).await.spawn(TaskContext::detached()).await;

    assert!(matches!(result, Err(TaskError::Fail { .. })));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Terminate,
            Call::Cleanup,
        ]
    );
}

#[tokio::test]
async fn termination_and_cleanup_failures_are_both_reported() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior {
        start_fails: true,
        terminate_fails: true,
        cleanup_fails: true,
    });
    handle.create_release.add_permits(1);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();

    let result = build(&runner).await.spawn(TaskContext::detached()).await;

    let error = result.unwrap_err().to_string();
    assert!(error.contains("container termination failed"));
    assert!(error.contains("cleanup failed"));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Terminate,
            Call::Cleanup,
        ]
    );
}

#[tokio::test]
async fn cleanup_failure_is_fatal() {
    let (engine, handle) = ControlledEngine::blocked(AttemptBehavior {
        start_fails: false,
        terminate_fails: false,
        cleanup_fails: true,
    });
    handle.create_release.add_permits(1);
    let runner = ContainerRunner::new("containerd", drop_releasing_engine(engine)).unwrap();

    let result = build(&runner).await.spawn(TaskContext::detached()).await;

    assert!(matches!(result, Err(TaskError::Fatal { .. })));
    assert_eq!(
        *handle.calls.lock().unwrap(),
        [
            Call::Create {
                attempt: 1,
                image: "docker.io/library/alpine:latest".into(),
            },
            Call::Start,
            Call::Wait,
            Call::Cleanup,
        ]
    );
}

#[tokio::test]
async fn output_drain_reports_reader_join_failure_without_task_failure() {
    let capture = Arc::new(TraceCapture::default());
    let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
    let mut output = OutputTasks {
        stdout: Some(tokio::spawn(async {
            panic!("container stdout reader failure")
        })),
        stderr: Some(tokio::spawn(async {})),
    };

    output
        .drain("container-output-join")
        .with_subscriber(dispatch)
        .await;

    let fields = capture.fields.lock().unwrap().join(" ");
    assert!(
        fields.contains("container.output_reader_failed"),
        "{fields}"
    );
    assert!(fields.contains("panicked"), "{fields}");
}

#[test]
fn attempt_counter_rejects_after_identity_limit() {
    let attempts = AtomicU32::new(u32::MAX - 1);

    assert_eq!(next_attempt(&attempts), Some(u32::MAX));
    assert_eq!(next_attempt(&attempts), None);
    assert_eq!(attempts.load(Ordering::Relaxed), u32::MAX);
}
