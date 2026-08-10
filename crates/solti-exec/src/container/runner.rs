//! Container runner lifecycle.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use solti_model::{ContainerSpec, Task, TaskWorkload, WORKLOAD_API_VERSION, WorkloadTypeMeta};
use solti_runner::{
    BuildContext, OutputPublisherHandle, RunId, Runner, RunnerError, RunnerErrorKind, RunnerType,
    merge_env,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::watch;
use tracing::{Instrument as _, debug, debug_span, trace, warn};

use super::{
    ContainerAttempt, ContainerEngine, ContainerEngineError, ContainerErrorClass,
    ContainerExitStatus, ContainerRequest, ContainerRunnerConfig,
};
use crate::{
    output::{LogConfig, StreamKind, log_stream},
    registration::validate_runner_name,
};

/// Runner for `solti.io/v1`, kind `Container` workloads.
pub struct ContainerRunner {
    name: String,
    engine: Arc<dyn ContainerEngine>,
    config: ContainerRunnerConfig,
}

impl ContainerRunner {
    /// Creates a container runner with default settings.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for an invalid name.
    pub fn new(
        name: impl Into<String>,
        engine: Arc<dyn ContainerEngine>,
    ) -> Result<Self, crate::ExecError> {
        Self::with_config(name, engine, ContainerRunnerConfig::new())
    }

    /// Creates a container runner with explicit settings.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for invalid settings.
    pub fn with_config(
        name: impl Into<String>,
        engine: Arc<dyn ContainerEngine>,
        config: ContainerRunnerConfig,
    ) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        Ok(Self {
            name,
            engine,
            config: config.prepare()?,
        })
    }
}

#[derive(Debug, Clone)]
struct ContainerTaskConfig {
    run_id: Arc<str>,
    resource_name: solti_model::TaskId,
    generation: u64,
    image: String,
    command: Option<Vec<String>>,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    process_policy: super::ContainerProcessPolicy,
}

impl ContainerTaskConfig {
    fn request(&self, attempt: u32) -> ContainerRequest {
        ContainerRequest {
            attempt_id: format!("{}-a{attempt}", self.run_id),
            task_name: self.resource_name.clone(),
            generation: self.generation,
            attempt,
            image: self.image.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            process_policy: self.process_policy.clone(),
        }
    }
}

struct TaskExecContext {
    engine: Arc<dyn ContainerEngine>,
    metrics: solti_runner::MetricsHandle,
    output_publisher: OutputPublisherHandle,
    task: ContainerTaskConfig,
    logger: LogConfig,
    attempt: AtomicU32,
}

impl Runner for ContainerRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Container")
                .expect("built-in workload GVK"),
        ]
    }

    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let ContainerSpec {
            image,
            command,
            args,
            env,
            ..
        } = match task.spec().workload() {
            TaskWorkload::Container(spec) => spec,
            other => {
                return Err(RunnerError::UnsupportedWorkload {
                    runner: self.name.clone(),
                    api_version: other.api_version().to_owned(),
                    kind: other.kind().to_owned(),
                });
            }
        };

        validate_process_input(command.as_deref(), args, env, ctx)?;

        let task_config = ContainerTaskConfig {
            run_id: Arc::from(run_id.name()),
            resource_name: task.name().clone(),
            generation: task.metadata().generation(),
            image: image.clone(),
            command: command.clone().filter(|command| !command.is_empty()),
            args: args.clone(),
            env: merge_env(env, ctx.env()),
            process_policy: self.config.process_policy().clone(),
        };

        trace!(
            event = "container.build",
            task_name = %task.name(),
            generation = task.metadata().generation(),
            slot = %task.slot(),
            run_id = %run_id.name(),
            image = %image,
            "building container task",
        );

        let exec = Arc::new(TaskExecContext {
            engine: Arc::clone(&self.engine),
            metrics: ctx.metrics().clone(),
            output_publisher: Arc::clone(ctx.output_publisher()),
            task: task_config,
            logger: self.config.logger(),
            attempt: AtomicU32::new(0),
        });
        let name = exec.task.run_id.to_string();
        Ok(TaskFn::arc(name, move |cancel: TaskContext| {
            let exec = Arc::clone(&exec);
            async move { run_container(exec, cancel).await }
        }))
    }
}

fn validate_process_input(
    command: Option<&[String]>,
    args: &[String],
    task_env: &solti_model::TaskEnv,
    build: &BuildContext,
) -> Result<(), RunnerError> {
    if command
        .into_iter()
        .flatten()
        .chain(args)
        .any(|value| value.contains('\0'))
    {
        return Err(RunnerError::InvalidSpec(
            "container command and arguments cannot contain NUL".into(),
        ));
    }

    for entry in task_env
        .into_iter()
        .map(|entry| (entry.key(), entry.value()))
        .chain(
            build
                .env()
                .into_iter()
                .map(|entry| (entry.key(), entry.value())),
        )
    {
        let (name, value) = entry;
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(RunnerError::InvalidSpec(format!(
                "invalid container environment variable name {name:?}"
            )));
        }
        if value.contains('\0') {
            return Err(RunnerError::InvalidSpec(format!(
                "container environment variable {name:?} contains NUL"
            )));
        }
    }
    Ok(())
}

async fn run_container(ctx: Arc<TaskExecContext>, cancel: TaskContext) -> Result<(), TaskError> {
    let attempt = ctx.attempt.fetch_add(1, Ordering::Relaxed) + 1;
    let request = ctx.task.request(attempt);
    let sink = ctx
        .output_publisher
        .sink_for(&ctx.task.resource_name, ctx.task.generation, attempt);

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let worker = run_container_worker(ctx, request, sink, cancel_rx);
    tokio::pin!(worker);

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = cancel_tx.send(true);
            match worker.await {
                Ok(()) | Err(TaskError::Canceled) => Err(TaskError::Canceled),
                Err(error) => Err(error),
            }
        }
        result = worker.as_mut() => {
            drop(cancel_tx);
            result
        }
    }
}

fn cancellation_requested(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow() || cancel.has_changed().is_err()
}

async fn run_container_worker(
    ctx: Arc<TaskExecContext>,
    request: ContainerRequest,
    sink: Option<solti_runner::OutputSink>,
    cancel: watch::Receiver<bool>,
) -> Result<(), TaskError> {
    let span = debug_span!(
        "container_attempt",
        event = "container.attempt",
        task_name = %ctx.task.resource_name,
        generation = ctx.task.generation,
        run_id = %ctx.task.run_id,
        attempt = request.attempt(),
    );
    run_container_attempt(ctx, request, sink, cancel)
        .instrument(span)
        .await
}

async fn run_container_attempt(
    ctx: Arc<TaskExecContext>,
    request: ContainerRequest,
    sink: Option<solti_runner::OutputSink>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), TaskError> {
    if cancellation_requested(&cancel) {
        return Err(TaskError::Canceled);
    }

    trace!(
        event = "container.lifecycle",
        stage = "creating",
        image = %request.image(),
        "creating container attempt",
    );

    let mut attempt = match ctx.engine.create_attempt(request).await {
        Ok(attempt) => attempt,
        Err(error) => {
            ctx.metrics.record_runner_error(
                RunnerType::Container,
                RunnerErrorKind::Custom("attempt_create_failed".into()),
            );
            if cancellation_requested(&cancel) {
                return Err(TaskError::Canceled);
            }
            return Err(map_engine_error(error));
        }
    };
    trace!(
        event = "container.lifecycle",
        stage = "created",
        "container attempt created"
    );

    let mut output = OutputTasks::start(
        attempt.as_mut(),
        Arc::clone(&ctx.task.run_id),
        ctx.logger,
        sink,
    );

    if cancellation_requested(&cancel) {
        cleanup_attempt(attempt.as_mut(), false).await?;
        output.drain(&ctx.task.run_id).await;
        return Err(TaskError::Canceled);
    }

    trace!(
        event = "container.lifecycle",
        stage = "starting",
        "container attempt starting"
    );
    if let Err(start_error) = attempt.start().await {
        ctx.metrics
            .record_runner_error(RunnerType::Container, RunnerErrorKind::SpawnFailed);
        let canceled = cancellation_requested(&cancel);
        let cleanup = cleanup_attempt(attempt.as_mut(), true).await;
        output.drain(&ctx.task.run_id).await;
        cleanup?;
        return if canceled {
            Err(TaskError::Canceled)
        } else {
            Err(map_engine_error(start_error))
        };
    }
    trace!(
        event = "container.lifecycle",
        stage = "started",
        "container attempt started"
    );

    let completion = tokio::select! {
        biased;
        changed = cancel.changed() => {
            let _ = changed;
            AttemptCompletion::Canceled
        }
        status = attempt.wait() => AttemptCompletion::Exited(status),
    };

    let result = match completion {
        AttemptCompletion::Canceled => {
            debug!(
                event = "container.cancellation",
                "cancellation requested; terminating container"
            );
            let terminate = attempt.terminate().await;
            let waited = attempt.wait().await;
            match (terminate, waited) {
                (Ok(()), Ok(_)) => Err(TaskError::Canceled),
                (Err(error), _) => Err(TaskError::fatal(format!(
                    "container termination failed: {error}"
                ))),
                (Ok(()), Err(error)) => Err(TaskError::fatal(format!(
                    "container wait after termination failed: {error}"
                ))),
            }
        }
        AttemptCompletion::Exited(Ok(status)) => {
            trace!(
                event = "container.lifecycle",
                stage = "exited",
                exit_code = status.code(),
                "container attempt exited"
            );
            evaluate_exit(status)
        }
        AttemptCompletion::Exited(Err(error)) => {
            let terminate_error = attempt.terminate().await.err();
            if let Some(terminate_error) = terminate_error {
                Err(TaskError::fatal(format!(
                    "container wait failed: {error}; termination failed: {terminate_error}"
                )))
            } else {
                Err(map_engine_error(error))
            }
        }
    };

    output.drain(&ctx.task.run_id).await;
    let cleanup = attempt.cleanup().await;
    if let Err(error) = cleanup {
        ctx.metrics.record_runner_error(
            RunnerType::Container,
            RunnerErrorKind::Custom("cleanup_failed".into()),
        );
        return Err(TaskError::fatal(format!(
            "container cleanup failed: {error}"
        )));
    }
    trace!(
        event = "container.lifecycle",
        stage = "cleaned",
        "container attempt cleaned"
    );
    result
}

async fn cleanup_attempt(
    attempt: &mut dyn ContainerAttempt,
    terminate: bool,
) -> Result<(), TaskError> {
    let termination_error = if terminate {
        attempt.terminate().await.err()
    } else {
        None
    };
    let cleanup_error = attempt.cleanup().await.err();

    match (termination_error, cleanup_error) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(TaskError::fatal(format!(
            "container termination failed: {error}"
        ))),
        (None, Some(error)) => Err(TaskError::fatal(format!(
            "container cleanup failed: {error}"
        ))),
        (Some(termination), Some(cleanup)) => Err(TaskError::fatal(format!(
            "container termination failed: {termination}; cleanup failed: {cleanup}"
        ))),
    }
}

fn map_engine_error(error: ContainerEngineError) -> TaskError {
    match error.class() {
        ContainerErrorClass::Retryable => TaskError::fail_from(error),
        ContainerErrorClass::Permanent => TaskError::fatal_from(error),
    }
}

fn evaluate_exit(status: ContainerExitStatus) -> Result<(), TaskError> {
    if status.success() {
        Ok(())
    } else {
        Err(TaskError::fail(format!(
            "container exited with non-zero code: {}",
            status.code()
        ))
        .with_exit_code(status.code()))
    }
}

enum AttemptCompletion {
    Exited(Result<ContainerExitStatus, ContainerEngineError>),
    Canceled,
}

struct OutputTasks {
    stdout: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

impl OutputTasks {
    fn start(
        attempt: &mut dyn ContainerAttempt,
        run_id: Arc<str>,
        logger: LogConfig,
        sink: Option<solti_runner::OutputSink>,
    ) -> Self {
        let stdout = attempt.take_stdout().map(|reader| {
            let run_id = Arc::clone(&run_id);
            let sink = sink.clone();
            let span = tracing::Span::current();
            tokio::spawn(
                async move {
                    log_stream(reader, &run_id, StreamKind::Stdout, &logger, sink.as_ref()).await;
                }
                .instrument(span),
            )
        });
        let stderr = attempt.take_stderr().map(|reader| {
            let span = tracing::Span::current();
            tokio::spawn(
                async move {
                    log_stream(reader, &run_id, StreamKind::Stderr, &logger, sink.as_ref()).await;
                }
                .instrument(span),
            )
        });
        Self { stdout, stderr }
    }

    async fn drain(&mut self, run_id: &str) {
        #[cfg(not(test))]
        const GRACE: std::time::Duration = std::time::Duration::from_secs(5);
        #[cfg(test)]
        const GRACE: std::time::Duration = std::time::Duration::from_millis(100);
        let drained = tokio::time::timeout(GRACE, async {
            if let Some(stdout) = self.stdout.as_mut() {
                let _ = stdout.await;
            }
            if let Some(stderr) = self.stderr.as_mut() {
                let _ = stderr.await;
            }
        })
        .await
        .is_ok();

        if !drained {
            if let Some(stdout) = &self.stdout {
                stdout.abort();
            }
            if let Some(stderr) = &self.stderr {
                stderr.abort();
            }
            warn!(
                event = "container.output_drain_timed_out",
                run_id = %run_id,
                "container output drain timed out"
            );
        }
    }
}

impl Drop for OutputTasks {
    fn drop(&mut self) {
        if let Some(stdout) = self.stdout.take() {
            stdout.abort();
        }
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
}

impl fmt::Debug for ContainerRunner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContainerRunner")
            .field("name", &self.name)
            .field("engine", &"<engine>")
            .field("config", &self.config)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex, atomic::AtomicBool},
        time::Duration,
    };

    use async_trait::async_trait;
    use solti_model::{ContainerSpec, TaskEnv, TaskSpec};
    use taskvisor::{
        RuntimeError, Supervisor, SupervisorConfig, TaskOutcomeKind, TaskSpec as TaskvisorTaskSpec,
    };
    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::container::{ContainerEngineInfo, ContainerOutput};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Probe,
        Create { attempt: u32, image: String },
        Start,
        Wait,
        Terminate,
        Cleanup,
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

    fn task() -> Task {
        let workload = TaskWorkload::Container(ContainerSpec::new(
            "docker.io/library/alpine:latest".into(),
            Some(vec!["echo".into()]),
            vec!["hello".into()],
            TaskEnv::new(),
        ));
        let spec = TaskSpec::builder("container-slot", workload, 5_000_u64)
            .build()
            .unwrap();
        Task::new("container-task", spec).unwrap()
    }

    fn build(runner: &ContainerRunner) -> TaskRef {
        let resource = task();
        let run_id = solti_runner::make_run_id(runner.name(), resource.slot().as_str());
        runner
            .build_task(&resource, &run_id, &BuildContext::default())
            .unwrap()
    }

    #[tokio::test]
    async fn build_has_no_engine_io_and_each_spawn_gets_one_attempt() {
        let (engine, calls) = FakeEngine::new(0);
        let runner = ContainerRunner::new("containerd", engine).unwrap();
        let task = build(&runner);

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
    async fn pre_canceled_attempt_performs_no_engine_io() {
        let (engine, calls) = FakeEngine::new(0);
        let runner = ContainerRunner::new("containerd", engine).unwrap();

        let error = build(&runner)
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
            FakeEngine::failing(ContainerErrorClass::Retryable),
        )
        .unwrap();
        assert!(matches!(
            build(&retryable).spawn(TaskContext::detached()).await,
            Err(TaskError::Fail { .. })
        ));

        let permanent = ContainerRunner::new(
            "permanent",
            FakeEngine::failing(ContainerErrorClass::Permanent),
        )
        .unwrap();
        assert!(matches!(
            build(&permanent).spawn(TaskContext::detached()).await,
            Err(TaskError::Fatal { .. })
        ));

        let (engine, _) = FakeEngine::new(23);
        let failed = ContainerRunner::new("exit-code", engine).unwrap();
        assert!(matches!(
            build(&failed).spawn(TaskContext::detached()).await,
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
    async fn dropping_outer_future_drops_in_flight_create() {
        let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
        let runner = ContainerRunner::new("containerd", engine).unwrap();
        let task = build(&runner);
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
    async fn taskvisor_force_abort_drops_lifecycle_before_shutdown_returns() {
        let (engine, handle) = ControlledEngine::blocked(AttemptBehavior::default());
        let runner = ContainerRunner::new("containerd", engine).unwrap();
        let supervisor =
            Supervisor::new(SupervisorConfig::new().with_grace(Duration::ZERO), vec![]);
        let supervisor_handle = supervisor.serve();
        let (_, waiter) = supervisor_handle
            .add_and_watch(TaskvisorTaskSpec::once(build(&runner)))
            .await
            .unwrap();

        wait_for(&handle.create_entered, "create attempt to start").await;
        assert!(handle.create_in_flight.load(Ordering::SeqCst));

        let shutdown = supervisor_handle.shutdown().await;
        assert!(matches!(shutdown, Err(RuntimeError::GraceExceeded { .. })));
        let outcome = waiter.wait().await.unwrap();
        assert_eq!(outcome.kind(), TaskOutcomeKind::ForceAborted);
        assert!(!handle.create_in_flight.load(Ordering::SeqCst));
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
        let runner = ContainerRunner::new("containerd", engine).unwrap();
        let supervisor = Supervisor::new(SupervisorConfig::new(), vec![]);
        let supervisor_handle = supervisor.serve();
        let (_, waiter) = supervisor_handle
            .add_and_watch(
                TaskvisorTaskSpec::once(build(&runner)).with_timeout(Duration::from_millis(20)),
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

    #[tokio::test]
    async fn start_failure_terminates_and_cleans_attempt() {
        let (engine, handle) = ControlledEngine::blocked(AttemptBehavior {
            start_fails: true,
            terminate_fails: false,
            cleanup_fails: false,
        });
        handle.create_release.add_permits(1);
        let runner = ContainerRunner::new("containerd", engine).unwrap();

        let result = build(&runner).spawn(TaskContext::detached()).await;

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
        let runner = ContainerRunner::new("containerd", engine).unwrap();

        let result = build(&runner).spawn(TaskContext::detached()).await;

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
        let runner = ContainerRunner::new("containerd", engine).unwrap();

        let result = build(&runner).spawn(TaskContext::detached()).await;

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
}
