//! # Subprocess execution
//!
//! [`SubprocessRunner`] converts a `Subprocess` resource into a Taskvisor task.
//! It resolves immutable settings during the build.
//! It creates operating-system resources inside each attempt.
//!
//! ## Attempt Lifecycle
//!
//! ```text
//! start attempt
//!      ▼
//! output sink + optional cgroup + optional script descriptor
//!      ▼
//! process domain
//!   ├── stdout/stderr ──► output sink + optional tracing copy
//!   ├── exit ───────────► cgroup + process group + leader
//!   └── cancellation ───► cgroup + process group + leader
//!      ▼
//! release attempt-scoped resources
//! ```
//!
//! On Unix, each attempt owns a session and process group.
//! Termination signals the dedicated process group and a running leader.
//! A configured cgroup is terminated too.
//! Normal completion applies the same boundary before leader reap.
//! Other platforms stop the leader process.
//! Stdout and stderr reader tasks are attempt-owned. Normal completion drains
//! them within a fixed grace period. Dropping the attempt future aborts them
//! and releases their pipe endpoints.

use std::{
    fmt,
    future::Future,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
};

use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tracing::{Instrument as _, debug, debug_span, trace, warn};

use solti_model::{SubprocessSpec, Task, TaskWorkload, WORKLOAD_API_VERSION, WorkloadTypeMeta};
use solti_runner::{
    BuildContext, OutputPublisherHandle, OutputSink, RunId, Runner, RunnerError, RunnerErrorKind,
    RunnerType, merge_env, record_runner_error, request_output_sink,
};

use crate::subprocess::{
    backend::{PreparedSubprocessBackendConfig, SubprocessBackendConfig},
    boundary::PinnedCwd,
    child::{ChildOutput, ProcessChild},
    cwd_domain::{CwdDomain, CwdPinError},
    domain::{
        ActiveProcessDomain, AttachedProcessOwnership, DropFinalizerDomain,
        DropFinalizerReservation, PreparedProcessOwnership, SubprocessFinalizerStatus,
    },
    script::AnonymousScript,
    task::SubprocessTaskConfig,
};
use crate::{
    output::{LogConfig, OutputDrain, OutputTasks, StreamKind, log_stream},
    registration::validate_runner_name,
};

#[cfg(unix)]
use crate::subprocess::exec_unix::ExecvePlan;

/// Runner that executes [`TaskWorkload::Subprocess`] as OS subprocesses.
///
/// The runner declares the built-in `solti.io/v1`, kind `Subprocess` workload.
/// Its name becomes part of run IDs and the automatic runner label.
///
/// ## Attempt Results
///
/// | Event                                  | Result                       |
/// |----------------------------------------|------------------------------|
/// | Exit code `0`                          | Success                      |
/// | Non-zero with `fail_on_non_zero`       | Retryable failure            |
/// | Non-zero without `fail_on_non_zero`    | Success                      |
/// | Cooperative cancellation               | `TaskError::Canceled`        |
/// | Process-domain lifecycle error          | Fatal failure                |
/// | Permanent operating-system error       | Fatal failure                |
/// | Other operating-system error           | Retryable failure            |
///
/// On Unix, one attempt owns one session and process group.
/// The runner waits up to five seconds for output pipes after the leader exits.
/// It then signals descendants that remain inside its cgroup or process group.
/// Cancellation wins a tie with leader exit and remains latched while output,
/// reap, and cleanup ownership is discharged. A physical lifecycle failure is
/// still fatal and is not hidden by cancellation.
///
/// The runner owns the wait status of every child it starts.
/// A dropped task future moves the child and host domain to one reaper worker.
/// The worker does not depend on the attempt's Tokio runtime.
/// Cleanup admission is reserved before script, cgroup, or process resources.
/// Build-time cwd resolution runs on a separate bounded runner-owned worker.
/// Keep the concrete runner and call [`shutdown`](Self::shutdown) after its
/// supervisors and task references have stopped.
/// The embedding process must not reap arbitrary children or enable automatic `SIGCHLD` reaping.
/// On Linux, a configured target user outside the agent's real and effective
/// user IDs, or a child policy retaining `CAP_SETUID`, requires effective
/// parent `CAP_KILL` on construction, executor, and finalizer threads.
///
/// ## See Also
///
/// - [`SubprocessBackendConfig`]
/// - [`register_subprocess_runner`](super::register_subprocess_runner)
/// - [`solti_runner::Runner`]
pub struct SubprocessRunner {
    /// Runner name.
    name: String,
    /// Backend configuration applied to all tasks spawned by this runner.
    config: Arc<PreparedSubprocessBackendConfig>,
    /// Bounded active and deferred process ownership for this runner.
    finalizer: DropFinalizerDomain,
    /// Bounded runner-owned blocking cwd preparation.
    cwd_domain: CwdDomain,
}

/// Builds a cgroup name for one task build.
fn build_cgroup_name(runner: &str, slot: &str, seq: u64, timestamp: u64) -> String {
    format!("{runner}-{slot}-{seq:x}-{timestamp:x}")
}

/// Combines independently owned subprocess shutdown domains without dropping
/// either failure.
fn combine_runner_shutdown(
    cleanup: std::io::Result<()>,
    cwd: std::io::Result<()>,
) -> std::io::Result<()> {
    match (cleanup, cwd) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(cleanup), Err(cwd)) => Err(std::io::Error::other(format!(
            "subprocess cleanup shutdown failed: {cleanup}; cwd I/O shutdown failed: {cwd}"
        ))),
    }
}

/// Allocates a unique one-based attempt number without wrapping identity.
fn next_attempt(attempts: &AtomicU32) -> Option<u32> {
    attempts
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |attempt| {
            attempt.checked_add(1)
        })
        .ok()
        .and_then(|attempt| attempt.checked_add(1))
}

impl SubprocessRunner {
    /// Creates a subprocess runner with default backend settings.
    ///
    /// The default uses an empty [`crate::host::HostProcessPolicy`].
    /// `name` is used in run IDs, cgroup paths, and the automatic runner label.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for an invalid name.
    /// Returns [`crate::ExecError::Io`] when the cleanup or cwd worker cannot
    /// start.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let config = SubprocessBackendConfig::new().prepare()?;
        let capacity = config.prepared_cleanup_capacity();
        let finalizer = DropFinalizerDomain::start(capacity)?;
        let cwd_domain = CwdDomain::start(capacity)?;
        Ok(Self {
            name,
            config: Arc::new(config),
            finalizer,
            cwd_domain,
        })
    }

    /// Creates a subprocess runner with explicit backend settings.
    ///
    /// Configuration paths are resolved during this call.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for invalid settings
    /// or missing Linux process-group termination authority. See
    /// [`SubprocessBackendConfig::with_host_process_policy`].
    /// Returns [`crate::ExecError::Io`] when host resource preparation or the
    /// cleanup or cwd worker fails.
    pub fn with_config(
        name: impl Into<String>,
        config: SubprocessBackendConfig,
    ) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let config = config.prepare()?;
        let capacity = config.prepared_cleanup_capacity();
        let finalizer = DropFinalizerDomain::start(capacity)?;
        let cwd_domain = CwdDomain::start(capacity)?;
        Ok(Self {
            name,
            config: Arc::new(config),
            finalizer,
            cwd_domain,
        })
    }

    /// Returns the finalizer's admission, health, and ownership counters.
    pub fn finalizer_status(&self) -> SubprocessFinalizerStatus {
        self.finalizer.status()
    }

    /// Closes cleanup and cwd admission and waits for accepted ownership.
    ///
    /// Call this after every supervisor and task reference that uses this
    /// runner has stopped. The operation is terminal and idempotent. A
    /// cancelled or timed-out call leaves admission closed; another call may
    /// continue the same shutdown.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::Io`] when the deadline expires, cleanup is
    /// quarantined, or either worker loses forward progress.
    pub async fn shutdown(&self, timeout: StdDuration) -> Result<(), crate::ExecError> {
        let (finalizer, cwd) = tokio::join!(
            self.finalizer.shutdown(timeout),
            self.cwd_domain.shutdown(timeout),
        );
        combine_runner_shutdown(finalizer, cwd).map_err(Into::into)
    }

    /// Builds immutable task settings from a resource.
    ///
    /// The returned settings are reused by every attempt.
    async fn build_task_config(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
    ) -> Result<BuiltSubprocessTask, RunnerError> {
        let spec = task.spec();
        let (cfg, script_body) = match spec.workload() {
            TaskWorkload::Subprocess(SubprocessSpec {
                mode,
                env,
                cwd,
                fail_on_non_zero,
                ..
            }) => {
                let max_body = self.config.max_script_body_bytes();
                let Resolved {
                    command,
                    args,
                    script_body,
                } = Self::resolve_mode(mode, max_body)?;
                let cfg = SubprocessTaskConfig {
                    seq: run_id.seq(),
                    run_id: Arc::from(run_id.name()),
                    fail_on_non_zero: *fail_on_non_zero,
                    env: merge_env(env, ctx.env()),
                    cwd: cwd.clone(),
                    command,
                    args,
                };
                (cfg, script_body)
            }
            other => {
                return Err(RunnerError::UnsupportedWorkload {
                    runner: self.name.clone(),
                    api_version: other.api_version().to_owned(),
                    kind: other.kind().to_owned(),
                });
            }
        };
        cfg.validate().map_err(RunnerError::InvalidSpec)?;
        let pinned_cwd = self
            .cwd_domain
            .pin(Arc::clone(&self.config), cfg.cwd.clone(), cancellation)
            .await
            .map_err(|error| match error {
                CwdPinError::InvalidSpec(error) => RunnerError::InvalidSpec(error),
                CwdPinError::Cancelled => RunnerError::BuildCancelled,
                CwdPinError::Unavailable(error) => RunnerError::Internal(error),
            })?;
        Ok(BuiltSubprocessTask {
            task: cfg,
            script_body,
            pinned_cwd,
        })
    }

    /// Resolves a subprocess mode into a command and arguments.
    ///
    /// Script bodies are decoded and checked here.
    /// Script transport remains attempt-scoped.
    fn resolve_mode(
        mode: &solti_model::SubprocessMode,
        max_script_body_bytes: usize,
    ) -> Result<Resolved, RunnerError> {
        match mode {
            solti_model::SubprocessMode::Command { command, args } => Ok(Resolved {
                command: command.clone(),
                args: args.clone(),
                script_body: None,
            }),
            solti_model::SubprocessMode::Script {
                interpreter, args, ..
            } => {
                let script = mode
                    .decode_body_with_limit(max_script_body_bytes)
                    .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;

                Ok(Resolved {
                    command: interpreter.clone(),
                    args: args.clone(),
                    script_body: Some(Arc::from(script)),
                })
            }
            _ => Err(RunnerError::InvalidSpec(
                "unsupported subprocess mode variant".into(),
            )),
        }
    }
}

/// Subprocess mode resolved during task construction.
struct Resolved {
    command: String,
    args: Vec<String>,

    /// Decoded script body.
    ///
    /// Command mode stores `None`.
    /// Script mode creates anonymous backing storage for each attempt.
    script_body: Option<Arc<str>>,
}

impl fmt::Debug for Resolved {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Resolved")
            .field("argument_count", &self.args.len())
            .field("script_present", &self.script_body.is_some())
            .finish()
    }
}

/// Immutable subprocess settings resolved while a task is built.
struct BuiltSubprocessTask {
    task: SubprocessTaskConfig,
    script_body: Option<Arc<str>>,
    pinned_cwd: Option<PinnedCwd>,
}

#[solti_runner::async_trait]
impl Runner for SubprocessRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess")
                .expect("built-in workload GVK"),
        ]
    }

    /// Builds a reusable [`TaskRef`] from a subprocess resource.
    ///
    /// This method resolves the mode and environment. It resolves and pins an
    /// explicit working directory on the runner-owned bounded cwd worker.
    /// It does not create script backing storage, a cgroup, or a process.
    /// Runner environment values from [`BuildContext`] override task values.
    /// Output and metrics also come from that context.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError::UnsupportedWorkload`] for another workload kind.
    /// Returns [`RunnerError::InvalidSpec`] when resolved process settings are invalid.
    /// This includes script decoding, script limits, environment values, and working-directory policy.
    /// Returns [`RunnerError::BuildCancelled`] when cancellation wins during
    /// cwd admission or preparation.
    async fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
        cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let BuiltSubprocessTask {
            task: task_cfg,
            script_body,
            pinned_cwd,
        } = self
            .build_task_config(task, run_id, ctx, cancellation)
            .await?;

        trace!(
            event = "subprocess.build",
            task_name = %task.name(),
            generation = task.metadata().generation(),
            slot = %task.slot(),
            run_id = %task_cfg.run_id,
            "building subprocess task",
        );

        let cgroup_name = self.config.has_cgroups().then(|| {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(StdDuration::from_secs(0))
                .as_secs();
            build_cgroup_name(&self.name, task.slot().as_str(), task_cfg.seq, timestamp)
        });

        let log_cfg = *self.config.log_config();

        let exec_ctx = Arc::new(TaskExecContext {
            task_cfg,
            runner_cfg: Arc::clone(&self.config),
            cgroup_name,
            metrics: ctx.metrics().clone(),
            log_cfg,
            output_publisher: Arc::clone(ctx.output_publisher()),
            attempt: AtomicU32::new(0),
            generation: task.metadata().generation(),
            resource_name: task.name().clone(),
            finalizer: self.finalizer.clone(),

            script_body,
            pinned_cwd,
        });

        let task: TaskRef = TaskFn::arc(move |cancel: TaskContext| {
            let ctx = Arc::clone(&exec_ctx);
            async move { run_subprocess(ctx, cancel).await }
        });
        Ok(task)
    }
}

/// Immutable execution context shared by all attempts.
struct TaskExecContext {
    runner_cfg: Arc<PreparedSubprocessBackendConfig>,
    metrics: solti_runner::MetricsHandle,
    output_publisher: OutputPublisherHandle,
    task_cfg: SubprocessTaskConfig,
    cgroup_name: Option<String>,
    log_cfg: LogConfig,
    attempt: AtomicU32,
    generation: u64,
    resource_name: solti_model::TaskId,
    finalizer: DropFinalizerDomain,

    /// Decoded script body.
    ///
    /// Script mode materializes fresh anonymous backing storage for every attempt.
    script_body: Option<Arc<str>>,
    /// Directory handle resolved while the task was built.
    pinned_cwd: Option<PinnedCwd>,
}

/// Builds the operating-system command for one attempt.
///
/// Script mode inserts `script_path` before the configured arguments.
fn build_command(ctx: &TaskExecContext, script_path: Option<&std::path::Path>) -> Command {
    let mut cmd = Command::new(&ctx.task_cfg.command);
    if let Some(path) = script_path {
        cmd.arg(path);
    }
    cmd.args(&ctx.task_cfg.args);
    if let Some(cwd) = &ctx.pinned_cwd {
        cwd.attach_to_command(&mut cmd);
    }
    #[cfg(not(unix))]
    apply_env_policy(&mut cmd, ctx);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // A dedicated session prevents the workload from joining the agent's
    // process group. `setsid` also makes the child its group leader.
    #[cfg(unix)]
    if !ctx.runner_cfg.starts_new_session() {
        attach_attempt_session(&mut cmd);
    }

    cmd
}

#[cfg(unix)]
fn attach_attempt_session(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: `setsid` is async-signal-safe and the hook captures no storage.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Builds the child environment from the configured [`EnvPolicy`](crate::subprocess::EnvPolicy).
///
/// Merged task and runner variables are applied last.
/// They take precedence over the selected parent policy.
/// `Clear` adds a [safe `PATH`] when the merged values do not set one.
/// `Allowlist` also requires that the allowlist does not name `PATH`.
///
/// [safe `PATH`]: crate::subprocess::backend::SAFE_DEFAULT_PATH
#[cfg(not(unix))]
fn apply_env_policy(cmd: &mut Command, ctx: &TaskExecContext) {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};

    let policy = ctx.runner_cfg.env_policy();

    let task_sets_path = ctx.task_cfg.env.contains_key("PATH");

    match policy {
        EnvPolicy::Inherit => {
            cmd.envs(&ctx.task_cfg.env);
        }
        EnvPolicy::Clear => {
            cmd.env_clear();
            if !task_sets_path {
                cmd.env("PATH", SAFE_DEFAULT_PATH);
            }
            cmd.envs(&ctx.task_cfg.env);
        }
        EnvPolicy::Allowlist(keys) => {
            cmd.env_clear();
            for key in keys {
                if let Some(val) = std::env::var_os(key) {
                    cmd.env(key, val);
                }
            }
            if !task_sets_path && !keys.iter().any(|k| k.as_str() == "PATH") {
                cmd.env("PATH", SAFE_DEFAULT_PATH);
            }
            cmd.envs(&ctx.task_cfg.env);
        }
    }
}

/// Materializes the exact Unix child environment for direct exec.
#[cfg(unix)]
fn unix_child_environment(ctx: &TaskExecContext) -> BTreeMap<OsString, OsString> {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};

    let mut environment = match ctx.runner_cfg.env_policy() {
        EnvPolicy::Inherit => std::env::vars_os().collect(),
        EnvPolicy::Clear | EnvPolicy::Allowlist(_) => BTreeMap::new(),
    };
    let task_sets_path = ctx.task_cfg.env.contains_key("PATH");

    match ctx.runner_cfg.env_policy() {
        EnvPolicy::Inherit => {}
        EnvPolicy::Clear => {
            if !task_sets_path {
                environment.insert("PATH".into(), SAFE_DEFAULT_PATH.into());
            }
        }
        EnvPolicy::Allowlist(keys) => {
            for key in keys {
                if let Some(value) = std::env::var_os(key) {
                    environment.insert(key.into(), value);
                }
            }
            if !task_sets_path && !keys.iter().any(|key| key.as_str() == "PATH") {
                environment.insert("PATH".into(), SAFE_DEFAULT_PATH.into());
            }
        }
    }
    environment.extend(
        ctx.task_cfg
            .env
            .iter()
            .map(|(key, value)| (key.into(), value.into())),
    );
    environment
}

/// Materializes argv entries after `argv[0]` for direct exec.
#[cfg(unix)]
fn unix_child_arguments(
    ctx: &TaskExecContext,
    script_path: Option<&std::path::Path>,
) -> Vec<OsString> {
    let mut arguments =
        Vec::with_capacity(ctx.task_cfg.args.len() + usize::from(script_path.is_some()));
    if let Some(script_path) = script_path {
        arguments.push(script_path.as_os_str().to_owned());
    }
    arguments.extend(ctx.task_cfg.args.iter().map(OsString::from));
    arguments
}

/// Adds operation context to an I/O error.
fn io_with_context(context: &str, source: std::io::Error) -> std::io::Error {
    std::io::Error::new(source.kind(), format!("{context}: {source}"))
}

fn io_error_is_permanent(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    #[cfg(unix)]
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOTDIR)
            | Some(libc::EISDIR)
            | Some(libc::ENOEXEC)
            | Some(libc::E2BIG)
            | Some(libc::EROFS)
    ) {
        return true;
    }
    false
}

fn task_io_error(context: &str, source: std::io::Error) -> TaskError {
    // Preserve raw errno classification before adding human-readable context.
    // `io::Error::new` intentionally does not retain `raw_os_error`.
    let permanent = io_error_is_permanent(&source);
    let error = io_with_context(context, source);
    if permanent {
        TaskError::fatal_from(error)
    } else {
        TaskError::fail_from(error)
    }
}

/// Failures that make starting another attempt unsafe.
#[derive(Debug, Default)]
struct ProcessLifecycleError {
    failures: Vec<(&'static str, std::io::Error)>,
}

impl ProcessLifecycleError {
    fn push(&mut self, operation: &'static str, error: std::io::Error) {
        self.failures.push((operation, error));
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for ProcessLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (operation, error)) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{operation} failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProcessLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failures
            .first()
            .map(|(_, error)| error as &(dyn std::error::Error + 'static))
    }
}

/// Script transport and prepared host ownership returned by blocking materialization.
///
/// Field order is semantic: cancellation drops the script before handing prepared
/// host resources to the finalizer.
struct MaterializedScriptOwnership {
    script: AnonymousScript,
    prepared: PreparedProcessOwnership,
}

impl MaterializedScriptOwnership {
    fn into_parts(self) -> (AnonymousScript, PreparedProcessOwnership) {
        (self.script, self.prepared)
    }
}

/// Creates one attempt's anonymous script transport outside the async runtime.
///
/// A creation or write failure ends the attempt.
async fn materialize_script(
    metrics: &solti_runner::MetricsHandle,
    body: Arc<str>,
    prepared: PreparedProcessOwnership,
) -> Result<MaterializedScriptOwnership, TaskError> {
    let written = tokio::task::spawn_blocking(move || {
        AnonymousScript::create(&body)
            .map(|script| MaterializedScriptOwnership { script, prepared })
    })
    .await;

    match written {
        Ok(Ok(file)) => Ok(file),
        Ok(Err(error)) => {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::SpawnFailed,
            );
            Err(task_io_error("script materialization failed", error))
        }
        Err(error) => {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::SpawnFailed,
            );
            Err(TaskError::fail(format!(
                "script materialization worker failed: {error}"
            )))
        }
    }
}

/// Prepares host process resources outside the async runtime.
fn prepare_host_process_ownership(
    backend: &PreparedSubprocessBackendConfig,
    cgroup_name: Option<&str>,
    reservation: DropFinalizerReservation,
) -> Result<PreparedProcessOwnership, crate::ExecError> {
    finish_host_process_prepare(
        backend.prepare_host_process_attempt_owned(cgroup_name),
        reservation,
    )
}

fn finish_host_process_prepare(
    prepared: Result<crate::host::PreparedHostProcessAttempt, crate::host::AttemptPrepareFailure>,
    reservation: DropFinalizerReservation,
) -> Result<PreparedProcessOwnership, crate::ExecError> {
    match prepared {
        Ok(prepared) => Ok(PreparedProcessOwnership::new(prepared, reservation)),
        Err(crate::host::AttemptPrepareFailure::Clean(error)) => Err(error.into()),
        Err(crate::host::AttemptPrepareFailure::Residual { error, cleanup }) => {
            match cleanup {
                Some(host) => reservation.submit_unspawned(host),
                None => reservation.quarantine_unrecoverable(),
            }
            Err(error.into())
        }
    }
}

/// Prepares host process resources outside the async runtime.
async fn prepare_backend(
    backend: &Arc<PreparedSubprocessBackendConfig>,
    metrics: &solti_runner::MetricsHandle,
    cgroup_name: Option<String>,
    reservation: DropFinalizerReservation,
) -> Result<PreparedProcessOwnership, TaskError> {
    if cgroup_name.is_none() {
        return prepare_host_process_ownership(backend, None, reservation).map_err(|error| {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::CgroupPrepareFailed,
            );
            TaskError::fatal(format!("host process resource preparation failed: {error}"))
        });
    }

    let backend = Arc::clone(backend);
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_host_process_ownership(&backend, cgroup_name.as_deref(), reservation)
    })
    .await;
    match prepared {
        Ok(Ok(prepared)) => Ok(prepared),
        Ok(Err(crate::ExecError::Io(error))) => {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::CgroupPrepareFailed,
            );
            Err(task_io_error("cgroup preparation failed", error))
        }
        Ok(Err(error)) => {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::CgroupPrepareFailed,
            );
            Err(TaskError::fatal(format!(
                "cgroup preparation failed: {error}"
            )))
        }
        Err(error) => {
            record_runner_error(
                metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::CgroupPrepareFailed,
            );
            Err(TaskError::fail(format!(
                "cgroup preparation worker failed: {error}"
            )))
        }
    }
}

fn cancellation_wins_pre_spawn<T>(
    cancel: &TaskContext,
    result: Result<T, TaskError>,
) -> Result<T, TaskError> {
    if cancel.is_cancelled() {
        Err(TaskError::Canceled)
    } else {
        result
    }
}

/// Attaches process state, rlimits, cgroup membership, and security controls.
fn apply_backend(
    cmd: &mut Command,
    ctx: &TaskExecContext,
    prepared: crate::host::PreparedHostProcessAttempt,
) -> crate::host::AttemptProcessDomain {
    ctx.runner_cfg.apply_to_command(cmd, prepared)
}

fn spawn_with_command(
    ctx: &TaskExecContext,
    script: Option<&AnonymousScript>,
    prepared: PreparedProcessOwnership,
    #[cfg(unix)] environment: &BTreeMap<OsString, OsString>,
) -> Result<
    (
        ProcessChild,
        crate::host::AttemptProcessDomain,
        DropFinalizerReservation,
    ),
    TaskError,
> {
    ctx.runner_cfg
        .validate_credential_termination_authority()
        .map_err(|error| {
            record_runner_error(
                &ctx.metrics,
                RunnerType::Subprocess,
                RunnerErrorKind::BackendConfigFailed,
            );
            TaskError::fatal(format!("host security preflight failed: {error}"))
        })?;

    let script_path = script.map(AnonymousScript::argument_path);
    let mut cmd = build_command(ctx, script_path);
    apply_fd_boundary(&mut cmd, ctx, script)?;
    #[cfg(unix)]
    let direct_exec = {
        let arguments = unix_child_arguments(ctx, script_path);

        // Keep `Command` and the final execve hook on one exact environment
        // snapshot. This preserves inherited non-UTF-8 entries too.
        cmd.env_clear();
        cmd.envs(environment);
        ExecvePlan::prepare(OsStr::new(&ctx.task_cfg.command), &arguments, environment).map_err(
            |error| {
                record_runner_error(
                    &ctx.metrics,
                    RunnerType::Subprocess,
                    RunnerErrorKind::SpawnFailed,
                );
                task_io_error("direct executable preparation failed", error)
            },
        )?
    };
    let (prepared, reservation) = prepared.into_parts();
    let host_process_domain = apply_backend(&mut cmd, ctx, prepared);
    #[cfg(unix)]
    direct_exec.attach(&mut cmd);
    let ownership = AttachedProcessOwnership::new(host_process_domain, reservation);
    let child = cmd.spawn().map_err(|error| {
        record_runner_error(
            &ctx.metrics,
            RunnerType::Subprocess,
            RunnerErrorKind::SpawnFailed,
        );
        task_io_error("spawn failed", error)
    })?;
    let (host_process_domain, reservation) = ownership.into_parts();
    Ok((child.into(), host_process_domain, reservation))
}

#[cfg(target_os = "macos")]
enum MacosSpawnAttempt {
    Spawned(ProcessChild, crate::host::AttemptProcessDomain),
    Fallback {
        prepared: crate::host::PreparedHostProcessAttempt,
        reason: MacosFallbackReason,
    },
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum MacosFallbackReason {
    HostControls,
    PinnedCwdUnsupported,
    CommandCompatibility,
}

#[cfg(target_os = "macos")]
impl MacosFallbackReason {
    fn as_label(self) -> &'static str {
        match self {
            Self::HostControls => "host_controls",
            Self::PinnedCwdUnsupported => "pinned_cwd_unsupported",
            Self::CommandCompatibility => "command_compatibility",
        }
    }
}

#[cfg(target_os = "macos")]
fn try_spawn_macos(
    ctx: &TaskExecContext,
    script: Option<&AnonymousScript>,
    prepared: crate::host::PreparedHostProcessAttempt,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<MacosSpawnAttempt, TaskError> {
    let Some(reset_signals) = prepared.macos_spawn_signals() else {
        return Ok(MacosSpawnAttempt::Fallback {
            prepared,
            reason: MacosFallbackReason::HostControls,
        });
    };

    let args = unix_child_arguments(ctx, script.map(AnonymousScript::argument_path));
    let mut passed_fds = ctx.runner_cfg.passed_fds();
    if let Some(script) = script {
        passed_fds.push(script.as_raw_fd());
    }
    let spec = crate::subprocess::spawn_macos::SpawnSpec {
        command: OsStr::new(&ctx.task_cfg.command),
        args: &args,
        env: environment,
        cwd: ctx.pinned_cwd.as_ref(),
        passed_fds: &passed_fds,
        reset_signals: &reset_signals,
    };
    if !crate::subprocess::spawn_macos::supports(&spec) {
        return Ok(MacosSpawnAttempt::Fallback {
            prepared,
            reason: MacosFallbackReason::PinnedCwdUnsupported,
        });
    }

    let child = crate::subprocess::spawn_macos::spawn(&spec).map_err(|error| {
        record_runner_error(
            &ctx.metrics,
            RunnerType::Subprocess,
            RunnerErrorKind::SpawnFailed,
        );
        task_io_error("native macOS spawn failed", error)
    })?;
    let Some(child) = child else {
        return Ok(MacosSpawnAttempt::Fallback {
            prepared,
            reason: MacosFallbackReason::CommandCompatibility,
        });
    };
    let domain = prepared.into_macos_spawn_domain();
    Ok(MacosSpawnAttempt::Spawned(child, domain))
}

/// Attaches the subprocess descriptor passlist.
///
/// The script descriptor is attempt-local.
/// The backend passlist contains descriptors owned by the runner.
#[cfg(unix)]
fn apply_fd_boundary(
    cmd: &mut Command,
    ctx: &TaskExecContext,
    script: Option<&AnonymousScript>,
) -> Result<(), TaskError> {
    let mut passed_fds = ctx.runner_cfg.passed_fds();
    if let Some(script) = script {
        passed_fds.push(script.as_raw_fd());
    }
    crate::host::attach_fd_cloexec(cmd.as_std_mut(), &passed_fds).map_err(|error| {
        record_runner_error(
            &ctx.metrics,
            RunnerType::Subprocess,
            RunnerErrorKind::SpawnFailed,
        );
        task_io_error("child file descriptor preparation failed", error)
    })
}

#[cfg(not(unix))]
fn apply_fd_boundary(
    _cmd: &mut Command,
    _ctx: &TaskExecContext,
    _script: Option<&AnonymousScript>,
) -> Result<(), TaskError> {
    Ok(())
}

/// Maps process exit status to the task result.
fn evaluate_exit(
    status: std::process::ExitStatus,
    task_cfg: &SubprocessTaskConfig,
) -> Result<(), TaskError> {
    if !status.success() && task_cfg.fail_on_non_zero.is_enabled() {
        let exit_code = status.code();
        let reason = match exit_code {
            Some(code) => format!("process exited with non-zero code: {code}"),
            None => "process terminated by signal".into(),
        };
        Err(TaskError::fail(reason).with_exit_code(exit_code))
    } else {
        debug!(
            event = "subprocess.exited",
            run_id = %task_cfg.run_id,
            outcome = "success",
            exit_code = ?status.code(),
            "subprocess exited"
        );
        Ok(())
    }
}

enum AttemptCompletion {
    LeaderExited(std::io::Result<()>),
    Canceled,
}

/// Observes the first logical completion signal for an active attempt.
///
/// This mirrors [`TaskContext::run_until_cancelled`]: cancellation wins when
/// both futures are ready in the same poll.
async fn observe_attempt_completion<F>(cancel: &TaskContext, observe_exit: F) -> AttemptCompletion
where
    F: Future<Output = std::io::Result<()>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            debug!(
                event = "subprocess.cancellation",
                "cancellation requested; terminating subprocess domain",
            );
            AttemptCompletion::Canceled
        }
        result = observe_exit => AttemptCompletion::LeaderExited(result),
    }
}

/// Sticky logical cancellation state for one attempt's physical teardown.
struct AttemptCancellationLatch<'a> {
    cancel: &'a TaskContext,
    latched: bool,
}

impl<'a> AttemptCancellationLatch<'a> {
    fn after_completion(cancel: &'a TaskContext, completion: &AttemptCompletion) -> Self {
        Self {
            cancel,
            latched: matches!(completion, AttemptCompletion::Canceled),
        }
    }

    /// Completes one mandatory post-exit ownership step while latching cancellation.
    ///
    /// Cancellation does not drop `operation`: output drain, leader reap, and
    /// host cleanup own physical resources and must finish. Their errors retain
    /// fatal precedence; the latch determines the result only after a
    /// successful lifecycle cleanup.
    async fn complete<F>(&mut self, operation: F) -> F::Output
    where
        F: Future,
    {
        if self.latched {
            return operation.await;
        }

        tokio::pin!(operation);
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                self.latched = true;
                operation.await
            }
            output = &mut operation => {
                // The token is sticky. This closes the ready-operation versus
                // cancellation race at the completion boundary.
                self.refresh();
                output
            }
        }
    }

    fn refresh(&mut self) {
        self.latched |= self.cancel.is_cancelled();
    }

    fn is_latched(&self) -> bool {
        self.latched
    }
}

/// Starts both subprocess readers and enrolls each spawned task immediately.
fn start_output_tasks(
    stdout: ChildOutput,
    stderr: ChildOutput,
    run_id: Arc<str>,
    logger: LogConfig,
    sink: Option<OutputSink>,
) -> OutputTasks {
    let mut tasks = OutputTasks::new();

    let stdout_run_id = Arc::clone(&run_id);
    let stdout_sink = sink.clone();
    let stdout_span = tracing::Span::current();
    tasks.spawn_stdout(
        async move {
            log_stream(
                stdout,
                &stdout_run_id,
                StreamKind::Stdout,
                &logger,
                stdout_sink.as_ref(),
            )
            .await;
        }
        .instrument(stdout_span),
    );

    let stderr_span = tracing::Span::current();
    tasks.spawn_stderr(
        async move {
            log_stream(stderr, &run_id, StreamKind::Stderr, &logger, sink.as_ref()).await;
        }
        .instrument(stderr_span),
    );
    tasks
}

fn report_output_drain(
    output: &mut OutputTasks,
    drain: OutputDrain,
    ctx: &TaskExecContext,
    attempt: u32,
) {
    match drain {
        OutputDrain::Completed { stdout, stderr } => {
            for (stream, failure) in [(StreamKind::Stdout, stdout), (StreamKind::Stderr, stderr)] {
                let Some(failure) = failure else {
                    continue;
                };
                warn!(
                    event = "subprocess.output_reader_failed",
                    task_name = %ctx.resource_name,
                    generation = ctx.generation,
                    run_id = %ctx.task_cfg.run_id,
                    attempt,
                    stream = stream.as_str(),
                    join_error = failure.as_str(),
                    "subprocess output reader task failed",
                );
            }
        }
        OutputDrain::TimedOut => {
            output.abort();
            warn!(
                event = "subprocess.output_drain_timed_out",
                task_name = %ctx.resource_name,
                generation = ctx.generation,
                run_id = %ctx.task_cfg.run_id,
                attempt,
                "subprocess output drain timed out after domain termination",
            );
        }
    }
}

/// Executes one subprocess attempt.
async fn run_subprocess(ctx: Arc<TaskExecContext>, cancel: TaskContext) -> Result<(), TaskError> {
    let Some(attempt) = next_attempt(&ctx.attempt) else {
        return Err(TaskError::fatal("subprocess attempt identity exhausted"));
    };
    let span = debug_span!(
        "subprocess_attempt",
        event = "subprocess.attempt",
        task_name = %ctx.resource_name,
        generation = ctx.generation,
        run_id = %ctx.task_cfg.run_id,
        attempt,
    );
    run_subprocess_attempt(ctx, cancel, attempt)
        .instrument(span)
        .await
}

async fn run_subprocess_attempt(
    ctx: Arc<TaskExecContext>,
    cancel: TaskContext,
    attempt: u32,
) -> Result<(), TaskError> {
    if cancel.is_cancelled() {
        return Err(TaskError::Canceled);
    }

    let sink = request_output_sink(
        &ctx.output_publisher,
        &ctx.resource_name,
        ctx.generation,
        attempt,
    );
    let drop_finalizer = ctx.finalizer.try_reserve().map_err(|error| {
        record_runner_error(
            &ctx.metrics,
            RunnerType::Subprocess,
            RunnerErrorKind::Custom("drop_finalizer_unavailable".into()),
        );
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            TaskError::fatal_from(io_with_context(
                "subprocess cleanup admission is unavailable",
                error,
            ))
        } else {
            task_io_error("subprocess cleanup admission is unavailable", error)
        }
    })?;

    // Command, args, and cwd are not logged: process input routinely carries
    // credentials and other secrets. The argument count is safe metadata.
    trace!(
        event = "subprocess.lifecycle",
        stage = "spawning",
        arg_count = ctx.task_cfg.args.len(),
        "spawning subprocess",
    );

    let cgroup_name = ctx
        .cgroup_name
        .as_ref()
        .map(|base| format!("{base}-{attempt:x}"));
    let prepared_host_process =
        prepare_backend(&ctx.runner_cfg, &ctx.metrics, cgroup_name, drop_finalizer).await;
    let prepared_host_process = cancellation_wins_pre_spawn(&cancel, prepared_host_process)?;

    // Script mode: prepare anonymous attempt-local backing storage.
    // The descriptor remains owned until the attempt ends.
    let (script, prepared_host_process) = match &ctx.script_body {
        Some(body) => {
            let materialized =
                materialize_script(&ctx.metrics, Arc::clone(body), prepared_host_process).await;
            let materialized = cancellation_wins_pre_spawn(&cancel, materialized)?;
            let (script, prepared) = materialized.into_parts();
            (Some(script), prepared)
        }
        None => (None, prepared_host_process),
    };

    // Preparation can yield while cancellation is requested. Keep the
    // prepared ownership local until this check: dropping it transfers any
    // unspawned host resources to the bounded finalizer.
    if cancel.is_cancelled() {
        return Err(TaskError::Canceled);
    }

    #[cfg(unix)]
    let child_environment = unix_child_environment(&ctx);

    #[cfg(target_os = "macos")]
    let (child, host_process_domain, drop_finalizer, spawn_backend) = {
        let (prepared, reservation) = prepared_host_process.into_parts();
        match try_spawn_macos(&ctx, script.as_ref(), prepared, &child_environment)? {
            MacosSpawnAttempt::Spawned(child, domain) => {
                (child, domain, reservation, "posix_spawn")
            }
            MacosSpawnAttempt::Fallback { prepared, reason } => {
                debug!(
                    event = "subprocess.spawn_fallback",
                    fallback_reason = reason.as_label(),
                    spawn_backend = "tokio_command",
                    "subprocess spawn fallback selected"
                );
                let prepared = PreparedProcessOwnership::new(prepared, reservation);
                let (child, domain, reservation) =
                    spawn_with_command(&ctx, script.as_ref(), prepared, &child_environment)?;
                (child, domain, reservation, "tokio_command")
            }
        }
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let (child, host_process_domain, drop_finalizer, spawn_backend) = {
        let (child, domain, reservation) = spawn_with_command(
            &ctx,
            script.as_ref(),
            prepared_host_process,
            &child_environment,
        )?;
        (child, domain, reservation, "tokio_command")
    };
    #[cfg(not(unix))]
    let (child, host_process_domain, drop_finalizer, spawn_backend) = {
        let (child, domain, reservation) =
            spawn_with_command(&ctx, script.as_ref(), prepared_host_process)?;
        (child, domain, reservation, "tokio_command")
    };

    let pid = child.id();
    let mut process = ActiveProcessDomain::new(
        child,
        host_process_domain,
        Arc::clone(&ctx.task_cfg.run_id),
        drop_finalizer,
    );

    trace!(
        event = "subprocess.lifecycle",
        stage = "spawned",
        spawn_backend,
        pid = ?pid,
        "subprocess spawned"
    );

    let stdout = process
        .take_stdout()
        .ok_or_else(|| TaskError::fatal("failed to capture stdout"))?;
    let stderr = process
        .take_stderr()
        .ok_or_else(|| TaskError::fatal("failed to capture stderr"))?;
    let mut output = start_output_tasks(
        stdout,
        stderr,
        Arc::clone(&ctx.task_cfg.run_id),
        ctx.log_cfg,
        sink,
    );

    let completion = observe_attempt_completion(&cancel, process.observe_exit()).await;
    let mut cancellation = AttemptCancellationLatch::after_completion(&cancel, &completion);

    let drained_before_termination = matches!(completion, AttemptCompletion::LeaderExited(Ok(())));
    let mut output_drain = if drained_before_termination {
        Some(cancellation.complete(output.drain()).await)
    } else {
        None
    };

    let termination_error = process.terminate().err();
    if let Some(error) = termination_error.as_ref() {
        warn!(
            event = "subprocess.termination_failed",
            task_name = %ctx.resource_name,
            generation = ctx.generation,
            run_id = %ctx.task_cfg.run_id,
            attempt,
            error = %error,
            "subprocess domain termination reported an error",
        );
    }

    let leader_status = if process.leader_can_be_reaped() {
        Some(cancellation.complete(process.reap()).await)
    } else {
        None
    };

    if !drained_before_termination {
        output_drain = Some(cancellation.complete(output.drain()).await);
    }
    report_output_drain(
        &mut output,
        output_drain.expect("one output drain path always runs"),
        &ctx,
        attempt,
    );

    let cleanup_error = if leader_status.as_ref().is_some_and(Result::is_ok) {
        cancellation.complete(process.cleanup()).await.err()
    } else {
        None
    };
    if let Some(error) = cleanup_error.as_ref() {
        warn!(
            event = "subprocess.cleanup_failed",
            task_name = %ctx.resource_name,
            generation = ctx.generation,
            run_id = %ctx.task_cfg.run_id,
            attempt,
            cgroup = ?process.cgroup_path(),
            error = %error,
            "failed to clean up subprocess domain",
        );
    }

    let observation_error = match completion {
        AttemptCompletion::Canceled | AttemptCompletion::LeaderExited(Ok(())) => None,
        AttemptCompletion::LeaderExited(Err(error)) => Some(error),
    };
    let (exit_status, reap_error) = match leader_status {
        Some(Ok(status)) => (Some(status), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };

    let mut lifecycle_error = ProcessLifecycleError::default();
    if let Some(error) = termination_error {
        lifecycle_error.push("process domain termination", error);
    }
    if let Some(error) = reap_error {
        lifecycle_error.push("leader reap", error);
    }
    if let Some(error) = cleanup_error {
        lifecycle_error.push("process domain cleanup", error);
    }
    if !lifecycle_error.is_empty() {
        if let Some(error) = observation_error {
            lifecycle_error
                .failures
                .insert(0, ("exit observation", error));
        }
        return Err(TaskError::fatal_from(lifecycle_error));
    }

    if let Some(error) = observation_error {
        return Err(task_io_error("exit observation failed", error));
    }
    // Cancellation is sticky and wins the final successful-lifecycle
    // boundary. Physical lifecycle errors above deliberately remain fatal.
    cancellation.refresh();
    if cancellation.is_latched() {
        return Err(TaskError::Canceled);
    }

    match exit_status {
        Some(status) => evaluate_exit(status, &ctx.task_cfg),
        None => Err(TaskError::fatal(
            "leader exit was observed without an available wait status",
        )),
    }
}

#[cfg(test)]
mod tests;
