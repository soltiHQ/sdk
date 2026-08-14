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
    ContainerAttempt, ContainerEngine, ContainerEngineBinding, ContainerEngineError,
    ContainerErrorClass, ContainerExitStatus, ContainerOwnershipContract, ContainerRequest,
    ContainerRunnerConfig,
};
use crate::{
    output::{LogConfig, StreamKind, log_stream},
    registration::validate_runner_name,
};

/// Runner for `solti.io/v1`, kind `Container` workloads.
///
/// Construction requires an explicit [`ContainerEngineBinding`].
/// The binding records the engine provider's dropped-lifecycle ownership contract.
pub struct ContainerRunner {
    name: String,
    engine: Arc<dyn ContainerEngine>,
    ownership_contract: ContainerOwnershipContract,
    config: ContainerRunnerConfig,
}

impl ContainerRunner {
    /// Creates a container runner with default settings.
    ///
    /// `engine` must carry an explicit ownership contract.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for an invalid name.
    pub fn new(
        name: impl Into<String>,
        engine: impl Into<ContainerEngineBinding>,
    ) -> Result<Self, crate::ExecError> {
        Self::with_config(name, engine.into(), ContainerRunnerConfig::new())
    }

    /// Creates a container runner with explicit settings.
    ///
    /// `engine` must carry an explicit ownership contract.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for invalid settings.
    pub fn with_config(
        name: impl Into<String>,
        engine: impl Into<ContainerEngineBinding>,
        config: ContainerRunnerConfig,
    ) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let (engine, ownership_contract) = engine.into().into_parts();
        Ok(Self {
            name,
            engine,
            ownership_contract,
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

#[solti_runner::async_trait]
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

    async fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
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
        "creating container attempt",
    );

    let mut attempt = match ctx.engine.create_attempt(request).await {
        Ok(attempt) => attempt,
        Err(error) => {
            ctx.metrics.record_runner_error(
                RunnerType::Container,
                RunnerErrorKind::Custom("attempt_create_failed".into()),
            );
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
            .field("ownership_contract", &self.ownership_contract)
            .field("config", &self.config)
            .finish()
    }
}

#[cfg(test)]
#[path = "runner/tests.rs"]
mod tests;
