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
//!   ├── stdout/stderr ──► tracing + output sink
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

use std::{
    fmt,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
};

use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tracing::{debug, trace, warn};

use solti_model::{SubprocessSpec, Task, TaskWorkload, WORKLOAD_API_VERSION, WorkloadTypeMeta};
use solti_runner::{
    BuildContext, OutputPublisherHandle, RunId, Runner, RunnerError, RunnerErrorKind, RunnerType,
    merge_env,
};

use crate::subprocess::{
    backend::{PreparedSubprocessBackendConfig, SubprocessBackendConfig},
    boundary::PinnedCwd,
    child::ProcessChild,
    domain::{ActiveProcessDomain, prepare_drop_finalizer},
    script::AnonymousScript,
    task::SubprocessTaskConfig,
};
use crate::{
    output::{LogConfig, StreamKind, log_stream},
    registration::validate_runner_name,
};

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
///
/// The runner owns the wait status of every child it starts.
/// A dropped task future moves the child and host domain to one reaper worker.
/// The worker does not depend on the attempt's Tokio runtime.
/// The embedding process must not reap arbitrary children or enable automatic `SIGCHLD` reaping.
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
}

/// Builds a cgroup name for one task build.
fn build_cgroup_name(runner: &str, slot: &str, seq: u64, timestamp: u64) -> String {
    format!("{runner}-{slot}-{seq:x}-{timestamp:x}")
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
    pub fn new(name: impl Into<String>) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let config = SubprocessBackendConfig::new().prepare()?;
        Ok(Self {
            name,
            config: Arc::new(config),
        })
    }

    /// Creates a subprocess runner with explicit backend settings.
    ///
    /// Configuration paths are resolved during this call.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for invalid settings.
    /// Returns [`crate::ExecError::Io`] when host resource preparation fails.
    pub fn with_config(
        name: impl Into<String>,
        config: SubprocessBackendConfig,
    ) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let config = config.prepare()?;
        Ok(Self {
            name,
            config: Arc::new(config),
        })
    }

    /// Builds immutable task settings from a resource.
    ///
    /// The returned settings are reused by every attempt.
    fn build_task_config(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
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
            .config
            .pin_cwd(cfg.cwd.as_deref())
            .map_err(RunnerError::InvalidSpec)?;
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
#[derive(Debug)]
struct Resolved {
    command: String,
    args: Vec<String>,

    /// Decoded script body.
    ///
    /// Command mode stores `None`.
    /// Script mode creates anonymous backing storage for each attempt.
    script_body: Option<Arc<str>>,
}

/// Immutable subprocess settings resolved while a task is built.
struct BuiltSubprocessTask {
    task: SubprocessTaskConfig,
    script_body: Option<Arc<str>>,
    pinned_cwd: Option<PinnedCwd>,
}

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
    /// This method resolves the mode, environment, and working directory.
    /// It pins an explicit working directory.
    /// It does not create script backing storage, a cgroup, or a process.
    /// Runner environment values from [`BuildContext`] override task values.
    /// Output and metrics also come from that context.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError::UnsupportedWorkload`] for another workload kind.
    /// Returns [`RunnerError::InvalidSpec`] when resolved process settings are invalid.
    /// This includes script decoding, script limits, environment values, and working-directory policy.
    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let BuiltSubprocessTask {
            task: task_cfg,
            script_body,
            pinned_cwd,
        } = self.build_task_config(task, run_id, ctx)?;

        trace!(
            resource = %task.name(),
            generation = task.metadata().generation(),
            slot = %task.slot(),
            taskvisor_task = %task_cfg.run_id,
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

            script_body,
            pinned_cwd,
        });

        let run_id = exec_ctx.task_cfg.run_id.to_string();
        let task: TaskRef = TaskFn::arc(run_id, move |cancel: TaskContext| {
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

/// Builds the child environment from the configured [`EnvPolicy`].
///
/// Merged task and runner variables are applied last.
/// They take precedence over the selected parent policy.
/// `Clear` adds a [safe `PATH`] when the merged values do not set one.
/// `Allowlist` also requires that the allowlist does not name `PATH`.
///
/// [safe `PATH`]: crate::subprocess::backend::SAFE_DEFAULT_PATH
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

/// Materializes the exact child environment for native macOS spawn.
#[cfg(target_os = "macos")]
fn macos_child_environment(ctx: &TaskExecContext) -> BTreeMap<OsString, OsString> {
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
    let error = io_with_context(context, source);
    if io_error_is_permanent(&error) {
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

/// Creates one attempt's anonymous script transport outside the async runtime.
///
/// A creation or write failure ends the attempt.
async fn materialize_script(
    ctx: &TaskExecContext,
    body: Arc<str>,
) -> Result<AnonymousScript, TaskError> {
    let written = tokio::task::spawn_blocking(move || AnonymousScript::create(&body)).await;

    match written {
        Ok(Ok(file)) => Ok(file),
        Ok(Err(error)) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
            Err(task_io_error("script materialization failed", error))
        }
        Err(error) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
            Err(TaskError::fail(format!(
                "script materialization worker failed: {error}"
            )))
        }
    }
}

/// Prepares host process resources outside the async runtime.
async fn prepare_backend(
    ctx: &TaskExecContext,
    cgroup_name: Option<String>,
) -> Result<crate::host::PreparedHostProcessAttempt, TaskError> {
    let backend = &ctx.runner_cfg;
    if cgroup_name.is_none() {
        return backend.prepare_host_process_attempt(None).map_err(|error| {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::CgroupPrepareFailed);
            TaskError::fatal(format!("host process resource preparation failed: {error}"))
        });
    }

    let backend = Arc::clone(backend);
    let prepared = tokio::task::spawn_blocking(move || {
        backend.prepare_host_process_attempt(cgroup_name.as_deref())
    })
    .await;
    match prepared {
        Ok(Ok(prepared)) => Ok(prepared),
        Ok(Err(crate::ExecError::Io(error))) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::CgroupPrepareFailed);
            Err(task_io_error("cgroup preparation failed", error))
        }
        Ok(Err(error)) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::CgroupPrepareFailed);
            Err(TaskError::fatal(format!(
                "cgroup preparation failed: {error}"
            )))
        }
        Err(error) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::CgroupPrepareFailed);
            Err(TaskError::fail(format!(
                "cgroup preparation worker failed: {error}"
            )))
        }
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
    prepared: crate::host::PreparedHostProcessAttempt,
) -> Result<(ProcessChild, crate::host::AttemptProcessDomain), TaskError> {
    let mut cmd = build_command(ctx, script.map(AnonymousScript::argument_path));
    apply_fd_boundary(&mut cmd, ctx, script)?;
    let host_process_domain = apply_backend(&mut cmd, ctx, prepared);
    let child = cmd.spawn().map_err(|error| {
        ctx.metrics
            .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        task_io_error("spawn failed", error)
    })?;
    Ok((child.into(), host_process_domain))
}

#[cfg(target_os = "macos")]
enum MacosSpawnAttempt {
    Spawned(ProcessChild, crate::host::AttemptProcessDomain),
    Fallback(crate::host::PreparedHostProcessAttempt),
}

#[cfg(target_os = "macos")]
fn try_spawn_macos(
    ctx: &TaskExecContext,
    script: Option<&AnonymousScript>,
    prepared: crate::host::PreparedHostProcessAttempt,
) -> Result<MacosSpawnAttempt, TaskError> {
    let Some(reset_signals) = prepared.macos_spawn_signals() else {
        return Ok(MacosSpawnAttempt::Fallback(prepared));
    };

    let mut args = Vec::with_capacity(ctx.task_cfg.args.len() + usize::from(script.is_some()));
    if let Some(script) = script {
        args.push(script.argument_path().as_os_str().to_owned());
    }
    args.extend(ctx.task_cfg.args.iter().map(OsString::from));
    let environment = macos_child_environment(ctx);
    let mut passed_fds = ctx.runner_cfg.passed_fds();
    if let Some(script) = script {
        passed_fds.push(script.as_raw_fd());
    }
    let spec = crate::subprocess::spawn_macos::SpawnSpec {
        command: OsStr::new(&ctx.task_cfg.command),
        args: &args,
        env: &environment,
        cwd: ctx.pinned_cwd.as_ref(),
        passed_fds: &passed_fds,
        reset_signals: &reset_signals,
    };
    if !crate::subprocess::spawn_macos::supports(&spec) {
        return Ok(MacosSpawnAttempt::Fallback(prepared));
    }

    let child = crate::subprocess::spawn_macos::spawn(&spec).map_err(|error| {
        ctx.metrics
            .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        task_io_error("native macOS spawn failed", error)
    })?;
    let Some(child) = child else {
        return Ok(MacosSpawnAttempt::Fallback(prepared));
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
        ctx.metrics
            .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
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
        debug!(task = %task_cfg.run_id, "subprocess exited successfully");
        Ok(())
    }
}

#[cfg(not(test))]
const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

enum AttemptCompletion {
    LeaderExited(std::io::Result<()>),
    Canceled,
}

async fn drain_output_tasks(
    stdout: &mut tokio::task::JoinHandle<()>,
    stderr: &mut tokio::task::JoinHandle<()>,
) -> bool {
    tokio::time::timeout(LOG_DRAIN_GRACE, async {
        let _ = tokio::join!(stdout, stderr);
    })
    .await
    .is_ok()
}

/// Executes one subprocess attempt.
async fn run_subprocess(ctx: Arc<TaskExecContext>, cancel: TaskContext) -> Result<(), TaskError> {
    let attempt = ctx.attempt.fetch_add(1, Ordering::Relaxed) + 1;
    let sink = ctx
        .output_publisher
        .sink_for(&ctx.resource_name, ctx.generation, attempt);
    let drop_finalizer = prepare_drop_finalizer().map_err(|error| {
        ctx.metrics.record_runner_error(
            RunnerType::Subprocess,
            RunnerErrorKind::Custom("drop_finalizer_unavailable".into()),
        );
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            TaskError::fatal_from(io_with_context(
                "subprocess drop finalizer is unavailable",
                error,
            ))
        } else {
            task_io_error("subprocess drop finalizer is unavailable", error)
        }
    })?;

    // Args and cwd are not logged: task arguments routinely carry tokens and
    // other secrets. Only the command name and the argument count are recorded.
    trace!(
        task = %ctx.task_cfg.run_id,
        command = %ctx.task_cfg.command,
        arg_count = ctx.task_cfg.args.len(),
        "spawning subprocess",
    );

    let cgroup_name = ctx
        .cgroup_name
        .as_ref()
        .map(|base| format!("{base}-{attempt:x}"));
    let prepared_host_process = prepare_backend(&ctx, cgroup_name).await?;

    // Script mode: prepare anonymous attempt-local backing storage.
    // The descriptor remains owned until the attempt ends.
    let script = match &ctx.script_body {
        Some(body) => Some(materialize_script(&ctx, Arc::clone(body)).await?),
        None => None,
    };

    #[cfg(target_os = "macos")]
    let (child, host_process_domain) =
        match try_spawn_macos(&ctx, script.as_ref(), prepared_host_process)? {
            MacosSpawnAttempt::Spawned(child, domain) => (child, domain),
            MacosSpawnAttempt::Fallback(prepared) => {
                spawn_with_command(&ctx, script.as_ref(), prepared)?
            }
        };
    #[cfg(not(target_os = "macos"))]
    let (child, host_process_domain) =
        spawn_with_command(&ctx, script.as_ref(), prepared_host_process)?;

    let mut process = ActiveProcessDomain::new(
        child,
        host_process_domain,
        Arc::clone(&ctx.task_cfg.run_id),
        drop_finalizer,
    );

    let log_cfg = ctx.log_cfg;

    let stdout = process
        .take_stdout()
        .ok_or_else(|| TaskError::fatal("failed to capture stdout"))?;
    let run_id_stdout = Arc::clone(&ctx.task_cfg.run_id);
    let sink_stdout = sink.clone();
    let mut stdout_task = tokio::spawn(async move {
        log_stream(
            stdout,
            &run_id_stdout,
            StreamKind::Stdout,
            &log_cfg,
            sink_stdout.as_ref(),
        )
        .await;
    });

    let stderr = process
        .take_stderr()
        .ok_or_else(|| TaskError::fatal("failed to capture stderr"))?;
    let run_id_stderr = Arc::clone(&ctx.task_cfg.run_id);
    let sink_stderr = sink.clone();
    let mut stderr_task = tokio::spawn(async move {
        log_stream(
            stderr,
            &run_id_stderr,
            StreamKind::Stderr,
            &log_cfg,
            sink_stderr.as_ref(),
        )
        .await;
    });

    let completion = tokio::select! {
        biased;
        result = process.observe_exit() => AttemptCompletion::LeaderExited(result),
        _ = cancel.cancelled() => {
            debug!(
                task = %ctx.task_cfg.run_id,
                "cancellation requested; terminating subprocess domain",
            );
            AttemptCompletion::Canceled
        }
    };

    let drained_before_termination = matches!(completion, AttemptCompletion::LeaderExited(Ok(())));
    let mut output_drained = false;
    if drained_before_termination {
        output_drained = drain_output_tasks(&mut stdout_task, &mut stderr_task).await;
    }

    let termination_error = process.terminate().err();
    if let Some(error) = termination_error.as_ref() {
        warn!(
            task = %ctx.task_cfg.run_id,
            error = %error,
            "subprocess domain termination reported an error",
        );
    }

    let leader_status = if process.leader_can_be_reaped() {
        Some(process.reap().await)
    } else {
        None
    };

    if !drained_before_termination {
        output_drained = drain_output_tasks(&mut stdout_task, &mut stderr_task).await;
    }
    if !output_drained {
        stdout_task.abort();
        stderr_task.abort();
        warn!(
            task = %ctx.task_cfg.run_id,
            "subprocess output drain timed out after domain termination",
        );
    }

    let cleanup_error = if leader_status.as_ref().is_some_and(Result::is_ok) {
        process.cleanup().await.err()
    } else {
        None
    };
    if let Some(error) = cleanup_error.as_ref() {
        warn!(
            task = %ctx.task_cfg.run_id,
            cgroup = ?process.cgroup_path(),
            error = %error,
            "failed to clean up subprocess domain",
        );
    }

    let (canceled, observation_error) = match completion {
        AttemptCompletion::Canceled => (true, None),
        AttemptCompletion::LeaderExited(Ok(())) => (false, None),
        AttemptCompletion::LeaderExited(Err(error)) => (false, Some(error)),
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
    if canceled {
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
mod tests {
    use super::*;

    #[test]
    fn process_lifecycle_errors_are_fatal_and_complete() {
        let mut lifecycle = ProcessLifecycleError::default();
        lifecycle.push(
            "process domain termination",
            std::io::Error::other("termination error"),
        );
        lifecycle.push(
            "leader reap",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "reap error"),
        );
        lifecycle.push(
            "process domain cleanup",
            std::io::Error::other("cleanup error"),
        );

        let error = TaskError::fatal_from(lifecycle);
        match error {
            TaskError::Fatal { reason, .. } => assert_eq!(
                reason,
                "process domain termination failed: termination error; leader reap failed: reap error; process domain cleanup failed: cleanup error"
            ),
            other => panic!("expected fatal lifecycle error, got {other}"),
        }
    }

    #[test]
    fn cgroup_name_is_stable() {
        assert_eq!(
            build_cgroup_name("runner", "slot", 42, 1000),
            "runner-slot-2a-3e8"
        );
    }

    type SinkCalls = Arc<std::sync::Mutex<Vec<(solti_model::TaskId, u64, u32)>>>;

    struct RecordingOutputPublisher {
        sender: std::sync::mpsc::Sender<solti_model::OutputEvent>,
        calls: SinkCalls,
    }

    impl solti_runner::OutputPublisher for RecordingOutputPublisher {
        fn sink_for(
            &self,
            task_name: &solti_model::TaskId,
            generation: u64,
            attempt: u32,
        ) -> Option<solti_runner::OutputSink> {
            self.calls
                .lock()
                .unwrap()
                .push((task_name.clone(), generation, attempt));
            let sender = self.sender.clone();
            Some(solti_runner::OutputSink::new(
                generation,
                attempt,
                move |event| {
                    let _ = sender.send(event);
                },
            ))
        }
    }

    fn recording_output_context() -> (
        BuildContext,
        std::sync::mpsc::Receiver<solti_model::OutputEvent>,
        SinkCalls,
    ) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let publisher: solti_runner::OutputPublisherHandle = Arc::new(RecordingOutputPublisher {
            sender,
            calls: Arc::clone(&calls),
        });
        (
            BuildContext::default().with_output_publisher(publisher),
            receiver,
            calls,
        )
    }

    #[cfg(unix)]
    async fn wait_for_recorded_pid(marker: &std::path::Path) -> Option<i32> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(marker)
                    && let Some(line) = value.trim().lines().next()
                    && let Ok(pid) = line.parse()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .ok()
    }

    #[cfg(unix)]
    async fn assert_process_gone(pid: i32) {
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if unsafe { libc::kill(pid, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();

        if !stopped {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("descendant process {pid} survived cleanup");
        }
    }

    fn mk_backoff() -> solti_model::BackoffPolicy {
        solti_model::BackoffPolicy {
            jitter: solti_model::JitterPolicy::Equal,
            first_ms: 100,
            max_ms: 1000,
            factor: 2.0,
        }
    }

    fn mk_subprocess_spec(slot: &str, command: &str) -> Task {
        mk_subprocess_spec_with_args(slot, command, &[])
    }

    fn mk_subprocess_spec_with_args(slot: &str, command: &str, args: &[&str]) -> Task {
        let spec = solti_model::TaskSpec::builder(
            slot,
            TaskWorkload::Subprocess(SubprocessSpec::new(
                solti_model::SubprocessMode::Command {
                    command: command.into(),
                    args: args.iter().map(|s| s.to_string()).collect(),
                },
                Default::default(),
                None,
                Default::default(),
            )),
            5_000u64,
        )
        .restart(solti_model::RestartPolicy::Never)
        .backoff(mk_backoff())
        .admission(solti_model::AdmissionPolicy::DropIfRunning)
        .build()
        .unwrap();
        Task::new(format!("task-{slot}"), spec).unwrap()
    }

    fn mk_script_spec(slot: &str, body: &[u8], args: &[&str]) -> Task {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = solti_model::TaskSpec::builder(
            slot,
            TaskWorkload::Subprocess(SubprocessSpec::new(
                solti_model::SubprocessMode::Script {
                    interpreter: "bash".into(),
                    body: BASE64.encode(body),
                    args: args.iter().map(|s| s.to_string()).collect(),
                },
                Default::default(),
                None,
                Default::default(),
            )),
            5_000u64,
        )
        .restart(solti_model::RestartPolicy::Never)
        .backoff(mk_backoff())
        .admission(solti_model::AdmissionPolicy::DropIfRunning)
        .build()
        .unwrap();
        Task::new(format!("task-{slot}"), spec).unwrap()
    }

    fn mk_embedded_spec(slot: &str) -> Task {
        let workload = TaskWorkload::Embedded(
            solti_model::EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
        );
        let spec = solti_model::TaskSpec::builder(slot, workload, 5_000u64)
            .restart(solti_model::RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(solti_model::AdmissionPolicy::DropIfRunning)
            .build()
            .unwrap();
        Task::new(format!("task-{slot}"), spec).unwrap()
    }

    fn build_with_run_id(
        runner: &SubprocessRunner,
        task: &Task,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let run_id = solti_runner::make_run_id(runner.name(), task.slot().as_str());
        runner.build_task(task, &run_id, ctx)
    }

    fn make_task_cfg() -> SubprocessTaskConfig {
        SubprocessTaskConfig {
            run_id: Arc::from("test-run-1"),
            seq: 1,
            command: "echo".into(),
            args: vec!["hello".into()],
            env: Default::default(),
            cwd: None,
            fail_on_non_zero: solti_model::Flag::default(),
        }
    }

    fn make_exec_ctx() -> TaskExecContext {
        TaskExecContext {
            task_cfg: make_task_cfg(),
            runner_cfg: Arc::new(SubprocessBackendConfig::new().prepare().unwrap()),
            cgroup_name: None,
            metrics: solti_runner::noop_metrics(),
            log_cfg: LogConfig::default(),
            output_publisher: solti_runner::noop_output_publisher(),
            attempt: AtomicU32::new(0),
            generation: 1,
            resource_name: solti_model::TaskId::new("test-resource").unwrap(),
            script_body: None,
            pinned_cwd: None,
        }
    }

    #[test]
    fn build_command_sets_args_and_pipes() {
        let ctx = make_exec_ctx();
        let cmd = build_command(&ctx, None);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "echo");
        let args: Vec<_> = std_cmd.get_args().collect();
        assert_eq!(args, vec!["hello"]);
    }

    #[cfg(target_os = "macos")]
    async fn run_through_macos_fallback(ctx: TaskExecContext) {
        let prepared = ctx.runner_cfg.prepare_host_process_attempt(None).unwrap();
        let prepared = match try_spawn_macos(&ctx, None, prepared).unwrap() {
            MacosSpawnAttempt::Fallback(prepared) => prepared,
            MacosSpawnAttempt::Spawned(_, _) => panic!("test case unexpectedly used native spawn"),
        };
        let (mut child, _domain) = spawn_with_command(&ctx, None, prepared).unwrap();
        assert!(child.wait().await.unwrap().success());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn executable_text_without_shebang_runs_through_fork_fallback() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::TempDir::new().unwrap();
        let program = directory.path().join("plain-text");
        std::fs::write(&program, b"exit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&program, permissions).unwrap();

        let mut ctx = make_exec_ctx();
        ctx.task_cfg.command = program.to_string_lossy().into_owned();
        ctx.task_cfg.args.clear();
        run_through_macos_fallback(ctx).await;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn relative_child_path_in_pinned_cwd_runs_through_fork_fallback() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::TempDir::new().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        std::fs::create_dir(canonical.join("bin")).unwrap();
        let program = canonical.join("bin/probe");
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&program, permissions).unwrap();

        let mut ctx = make_exec_ctx();
        ctx.task_cfg.command = "probe".into();
        ctx.task_cfg.args.clear();
        ctx.task_cfg.env.insert("PATH".into(), "bin".into());
        ctx.pinned_cwd = Some(PinnedCwd::open_absolute(&canonical).unwrap());
        run_through_macos_fallback(ctx).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_subprocess_owns_a_session() {
        let mut ctx = make_exec_ctx();
        ctx.task_cfg.command = "sleep".into();
        ctx.task_cfg.args = vec!["30".into()];
        let mut command = build_command(&ctx, None);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;

        // SAFETY: `getsid` only reads process metadata for a numeric pid.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);

        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn child_inherits_only_explicitly_passed_fd() {
        use std::os::fd::AsRawFd as _;

        let denied_file = tempfile::tempfile().unwrap();
        let denied_fd = denied_file.as_raw_fd();
        let mut denied_ctx = make_exec_ctx();
        denied_ctx.task_cfg.command = "test".into();
        denied_ctx.task_cfg.args = vec!["-e".into(), format!("/dev/fd/{denied_fd}")];
        let mut denied = build_command(&denied_ctx, None);
        apply_fd_boundary(&mut denied, &denied_ctx, None).unwrap();
        assert!(!denied.as_std_mut().status().unwrap().success());

        let passed_file = tempfile::tempfile().unwrap();
        let passed_fd = passed_file.as_raw_fd();
        let mut passed_ctx =
            ctx_with_backend(SubprocessBackendConfig::new().with_passed_fd(passed_file.into()));
        passed_ctx.task_cfg.command = "test".into();
        passed_ctx.task_cfg.args = vec!["-e".into(), format!("/dev/fd/{passed_fd}")];
        let mut passed = build_command(&passed_ctx, None);
        apply_fd_boundary(&mut passed, &passed_ctx, None).unwrap();
        assert!(passed.as_std_mut().status().unwrap().success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn new_session_policy_does_not_also_request_process_group() {
        use crate::host::{HostProcessPolicy, ProcessConfig};

        let mut ctx = ctx_with_backend(SubprocessBackendConfig::new().with_host_process_policy(
            HostProcessPolicy::new().with_process_config(ProcessConfig {
                new_session: true,
                ..Default::default()
            }),
        ));
        ctx.task_cfg.command = "sh".into();
        ctx.task_cfg.args = vec!["-c".into(), "exit 0".into()];

        let prepared = ctx.runner_cfg.prepare_host_process_attempt(None).unwrap();
        let mut command = build_command(&ctx, None);
        apply_fd_boundary(&mut command, &ctx, None).unwrap();
        let _guard = apply_backend(&mut command, &ctx, prepared);

        assert!(command.status().await.unwrap().success());
    }

    #[test]
    fn runner_name_validation_accepts_and_rejects() {
        for good in ["subprocess", "runner-1", "a.b_c", "x"] {
            assert!(validate_runner_name(good).is_ok(), "should accept {good:?}");
        }
        for bad in ["", ".", "..", "a/b", "a b", "runner\0", &"n".repeat(65)] {
            assert!(validate_runner_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn with_config_rejects_bad_runner_name() {
        let result = SubprocessRunner::with_config("bad/name", SubprocessBackendConfig::new());
        let err = result.err().expect("bad name must be rejected").to_string();
        assert!(err.contains("invalid runner name"), "got: {err}");
    }

    #[test]
    fn runner_accepts_a_dynamically_owned_name() {
        let suffix = 7;
        let name = format!("runner-{suffix}");
        let runner = SubprocessRunner::new(name.clone()).unwrap();

        assert_eq!(runner.name(), name);
    }

    #[test]
    fn build_command_prepends_script_path() {
        let ctx = make_exec_ctx();
        let cmd = build_command(&ctx, Some(std::path::Path::new("/tmp/solti-script-x")));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(args, vec!["/tmp/solti-script-x", "hello"]);
    }

    fn env_of(cmd: &Command) -> std::collections::HashMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn ctx_with_backend(cfg: SubprocessBackendConfig) -> TaskExecContext {
        let mut ctx = make_exec_ctx();
        ctx.runner_cfg = Arc::new(cfg.prepare().unwrap());
        ctx
    }

    #[test]
    fn env_inherit_injects_no_path() {
        use crate::subprocess::backend::EnvPolicy;

        let ctx =
            ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Inherit));
        let cmd = build_command(&ctx, None);
        assert!(!env_of(&cmd).contains_key("PATH"));
    }

    #[test]
    fn env_clear_injects_safe_path() {
        use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};
        let ctx =
            ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
        let cmd = build_command(&ctx, None);
        let env = env_of(&cmd);
        assert_eq!(env.get("PATH"), Some(&Some(SAFE_DEFAULT_PATH.to_string())));
    }

    #[test]
    fn env_clear_respects_task_provided_path() {
        use crate::subprocess::backend::EnvPolicy;
        let mut ctx =
            ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
        ctx.task_cfg
            .env
            .insert("PATH".into(), "/opt/custom/bin".into());
        let cmd = build_command(&ctx, None);
        assert_eq!(
            env_of(&cmd).get("PATH"),
            Some(&Some("/opt/custom/bin".to_string()))
        );
    }

    #[test]
    fn env_clear_keeps_task_vars() {
        use crate::subprocess::backend::EnvPolicy;
        let mut ctx =
            ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
        ctx.task_cfg.env.insert("FOO".into(), "bar".into());
        let cmd = build_command(&ctx, None);
        assert_eq!(env_of(&cmd).get("FOO"), Some(&Some("bar".to_string())));
    }

    #[test]
    fn env_allowlist_skips_absent_key_and_still_injects_path() {
        use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};
        // An allowlisted var that is not in the parent env is simply skipped;
        // PATH is still injected because neither the task nor the allowlist set it.
        let ctx = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(
            EnvPolicy::Allowlist(vec!["SOLTI_DEFINITELY_ABSENT_VAR_XYZ".into()]),
        ));
        let cmd = build_command(&ctx, None);
        assert_eq!(
            env_of(&cmd).get("PATH"),
            Some(&Some(SAFE_DEFAULT_PATH.to_string()))
        );
    }

    #[test]
    fn evaluate_exit_respects_fail_on_non_zero() {
        use std::process::Command as StdCommand;

        let success = StdCommand::new("true").status().unwrap();
        let failed = StdCommand::new("false").status().unwrap();
        let mut cfg = make_task_cfg();
        assert!(evaluate_exit(success, &cfg).is_ok());

        cfg.fail_on_non_zero = solti_model::Flag::disabled();
        assert!(evaluate_exit(failed, &cfg).is_ok());

        cfg.fail_on_non_zero = solti_model::Flag::enabled();
        let result = evaluate_exit(failed, &cfg);
        assert!(result.is_err());
        match result.unwrap_err() {
            TaskError::Fail {
                reason, exit_code, ..
            } => {
                assert!(reason.contains("non-zero"));
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected TaskError::Fail, got {other:?}"),
        }
    }

    #[test]
    fn build_task_returns_task_ref_for_subprocess() {
        let runner = SubprocessRunner::new("test-runner").unwrap();
        let task = mk_subprocess_spec("test-slot", "echo");
        let task_ref = build_with_run_id(&runner, &task, &BuildContext::default()).unwrap();

        assert_ne!(task_ref.name(), task.name().as_str());
        assert!(task_ref.name().starts_with("test-runner-test-slot-"));
    }

    #[test]
    fn build_task_rejects_non_subprocess_kind() {
        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_embedded_spec("test-slot");
        match build_with_run_id(&runner, &spec, &BuildContext::default()) {
            Err(RunnerError::UnsupportedWorkload {
                runner,
                api_version,
                kind,
            }) => {
                assert_eq!(runner, "test-runner");
                assert_eq!(api_version, "solti.io/v1");
                assert_eq!(kind, "Embedded");
            }
            Err(other) => panic!("expected UnsupportedWorkload, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn workload_types_declares_subprocess() {
        let runner = SubprocessRunner::new("test").unwrap();
        let task = mk_subprocess_spec("s", "echo");
        assert_eq!(
            runner.workload_types(),
            vec![task.spec().workload().type_meta()]
        );
    }

    #[tokio::test]
    async fn script_task_runs_and_streams_output() {
        use solti_model::OutputEvent;

        let (ctx, rx, _calls) = recording_output_context();

        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_script_spec("script-e2e", b"echo \"hello-$1\"", &["script"]);
        let task_ref = build_with_run_id(&runner, &spec, &ctx).unwrap();
        let cancel = TaskContext::detached();
        task_ref
            .spawn(cancel)
            .await
            .expect("script task must succeed");

        let found = rx.try_iter().any(|event| {
            let OutputEvent::Chunk(chunk) = event else {
                return false;
            };
            std::str::from_utf8(&chunk.line)
                .unwrap_or_default()
                .contains("hello-script")
        });
        assert!(
            found,
            "script output must reach the registry (anonymous transport created at run time, extra args preserved)"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires root with only CAP_SETUID and CAP_SETGID"]
    async fn script_task_runs_after_exact_credential_change() {
        use crate::host::{HostProcessPolicy, LinuxCapability, ProcessCredentials, SecurityConfig};

        assert_eq!(unsafe { libc::geteuid() }, 0, "test requires root");
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .map(str::trim)
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .expect("CapEff is missing from /proc/self/status");
        let has =
            |capability: LinuxCapability| effective & (1_u64 << capability.to_cap_value()) != 0;
        assert!(has(LinuxCapability::SetUid), "CAP_SETUID is required");
        assert!(has(LinuxCapability::SetGid), "CAP_SETGID is required");
        assert!(!has(LinuxCapability::Chown), "CAP_CHOWN must be absent");
        assert!(!has(LinuxCapability::FOwner), "CAP_FOWNER must be absent");
        if let Ok(setgroups) = std::fs::read_to_string("/proc/self/setgroups") {
            assert_ne!(setgroups.trim(), "deny", "setgroups must be permitted");
        }

        let backend = SubprocessBackendConfig::new().with_host_process_policy(
            HostProcessPolicy::new().with_security(SecurityConfig {
                credentials: Some(ProcessCredentials::new(65_534, 65_534)),
                no_new_privs: true,
                ..Default::default()
            }),
        );
        let runner = SubprocessRunner::with_config("credential-test", backend).unwrap();
        let build = BuildContext::default();
        let cancel = TaskContext::detached();

        let script = mk_script_spec("script-credentials", b"exit 0", &[]);
        let script_ref = build_with_run_id(&runner, &script, &build).unwrap();
        script_ref
            .spawn(cancel)
            .await
            .expect("sealed script must remain readable after changing credentials");
    }

    #[tokio::test]
    async fn script_task_can_be_spawned_repeatedly() {
        // Anonymous backing storage is materialized per attempt.
        // A retry must receive a fresh descriptor with the same body.
        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_script_spec("script-retry", b"exit 0", &[]);
        let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default()).unwrap();

        let ctx = TaskContext::detached();
        task_ref
            .spawn(ctx.clone())
            .await
            .expect("first attempt must succeed");
        task_ref
            .spawn(ctx)
            .await
            .expect("second attempt must succeed");
    }

    #[test]
    fn resolve_mode_command() {
        let mode = solti_model::SubprocessMode::Command {
            command: "ls".into(),
            args: vec!["-la".into()],
        };
        let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
        assert_eq!(r.command, "ls");
        assert_eq!(r.args, vec!["-la"]);
        assert!(r.script_body.is_none(), "Command mode carries no script");
    }

    #[test]
    fn resolve_mode_script_defers_transport_to_run_time() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mode = solti_model::SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode(b"echo hello"),
            args: vec!["extra".into()],
        };
        let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
        assert_eq!(r.command, "bash");
        assert_eq!(
            r.args,
            vec!["extra"],
            "resolve must not create backing storage: the descriptor path is prepended at spawn time"
        );

        let body = r.script_body.expect("Script mode must carry the body");
        assert_eq!(&*body, "echo hello");
    }

    #[test]
    fn resolve_mode_script_uses_explicit_interpreter() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mode = solti_model::SubprocessMode::Script {
            interpreter: "ruby".into(),
            body: BASE64.encode(b"puts 'hi'"),
            args: vec![],
        };
        let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
        assert_eq!(r.command, "ruby");
        assert!(r.args.is_empty());
        assert!(r.script_body.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_reaps_forked_grandchildren() {
        use std::process::Stdio;
        use tokio::process::Command as TokioCommand;

        let marker_dir = tempfile::TempDir::new().unwrap();
        let marker = marker_dir.path().join("pid");
        let marker_str = marker.to_string_lossy().to_string();

        let script = format!(
            r#"
            (sleep 60 & echo $! > "{marker}") &
            wait
            "#,
            marker = marker_str
        );

        let mut cmd = TokioCommand::new("bash");
        cmd.args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.process_group(0);

        let host = crate::host::HostProcessPolicy::new()
            .prepare()
            .unwrap()
            .prepare_attempt(None)
            .unwrap()
            .apply_to_command(cmd.as_std_mut());
        let child = cmd.spawn().expect("bash must spawn");
        let mut process = ActiveProcessDomain::new(
            child,
            host,
            Arc::from("test"),
            prepare_drop_finalizer().unwrap(),
        );
        let grandchild_pid = wait_for_recorded_pid(&marker).await;
        if let Some(pid) = grandchild_pid {
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "grandchild must be alive before cancel"
            );
        }

        process.terminate().unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), process.reap()).await;
        let grandchild_pid = grandchild_pid.expect("grandchild did not report its pid");
        assert_process_gone(grandchild_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_group() {
        // taskvisor enforces a per-attempt timeout via `tokio::time::timeout` and
        // force-abort via `JoinHandle::abort` — both DROP the task future without ever
        // polling the cooperative `cancel.cancelled()` branch.
        // The active process domain must still stop the process group on drop.
        let marker_dir = tempfile::TempDir::new().unwrap();
        let leader_marker = marker_dir.path().join("leader.pid");
        let descendant_marker = marker_dir.path().join("descendant.pid");
        let leader_marker = leader_marker.to_string_lossy().to_string();
        let descendant_marker = descendant_marker.to_string_lossy().to_string();

        // Record both identities before blocking on a long-lived descendant.
        let script = format!(
            r#"echo $$ > "{leader_marker}"; (sleep 60 & echo $! > "{descendant_marker}"); sleep 60"#
        );

        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("drop-slot", "bash", &["-c", &script]);
        let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default()).unwrap();

        let cancel = TaskContext::detached();
        let handle = tokio::spawn(async move { task_ref.spawn(cancel).await });
        let leader_pid = wait_for_recorded_pid(std::path::Path::new(&leader_marker)).await;
        let descendant_pid = wait_for_recorded_pid(std::path::Path::new(&descendant_marker)).await;

        handle.abort();
        let _ = handle.await;
        assert_process_gone(leader_pid.expect("leader did not report its pid")).await;
        assert_process_gone(descendant_pid.expect("descendant did not report its pid")).await;
    }

    #[test]
    fn resolve_mode_invalid_base64() {
        let mode = solti_model::SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "not-valid!!!".into(),
            args: vec![],
        };
        let err =
            SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap_err();
        assert!(matches!(err, RunnerError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn subprocess_streams_stdout_into_output_publisher() {
        use solti_model::OutputEvent;

        let (ctx, rx, calls) = recording_output_context();

        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("echo-slot", "echo", &["hello-stream"]);
        let task_ref = build_with_run_id(&runner, &spec, &ctx).unwrap();
        let cancel = TaskContext::detached();
        task_ref.spawn(cancel).await.expect("echo must succeed");

        let chunk = rx
            .try_iter()
            .find_map(|event| match event {
                OutputEvent::Chunk(chunk)
                    if std::str::from_utf8(&chunk.line)
                        .unwrap_or_default()
                        .contains("hello-stream") =>
                {
                    Some(chunk)
                }
                _ => None,
            })
            .expect("expected to receive 'hello-stream' line");
        assert_eq!(chunk.attempt, 1);
        assert_eq!(chunk.generation, 1);
        assert_eq!(chunk.stream, solti_model::StreamKind::Stdout);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(solti_model::TaskId::new("task-echo-slot").unwrap(), 1, 1)]
        );
    }

    #[tokio::test]
    async fn subprocess_attempt_counter_increments_on_each_spawn() {
        use solti_model::OutputEvent;

        let (ctx, rx, _calls) = recording_output_context();
        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("attempts-slot", "echo", &["x"]);
        let task_ref = build_with_run_id(&runner, &spec, &ctx).unwrap();
        let ctx = TaskContext::detached();
        task_ref.spawn(ctx.clone()).await.unwrap();
        task_ref.spawn(ctx).await.unwrap();

        let attempts: std::collections::BTreeSet<_> = rx
            .try_iter()
            .filter_map(|event| match event {
                OutputEvent::Chunk(chunk) => Some(chunk.attempt),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, std::collections::BTreeSet::from([1, 2]));
    }

    #[tokio::test]
    async fn attempt_is_allocated_before_spawn_failure() {
        let (ctx, _rx, calls) = recording_output_context();
        let runner = SubprocessRunner::new("test-runner").unwrap();
        let task = mk_subprocess_spec("failed-spawn", "/definitely/not/a/command");
        let task_ref = build_with_run_id(&runner, &task, &ctx).unwrap();

        assert!(task_ref.spawn(TaskContext::detached()).await.is_err());
        assert!(task_ref.spawn(TaskContext::detached()).await.is_err());

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[
                (solti_model::TaskId::new("task-failed-spawn").unwrap(), 1, 1,),
                (solti_model::TaskId::new("task-failed-spawn").unwrap(), 1, 2,),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemonized_grandchild_cannot_hold_output_open() {
        let runner = SubprocessRunner::new("hang-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("hang-slot", "sh", &["-c", "sleep 30 & exit 0"]);
        let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default()).unwrap();

        let ctx = TaskContext::detached();
        tokio::time::timeout(std::time::Duration::from_secs(2), task_ref.spawn(ctx))
            .await
            .expect("output drain must be bounded")
            .expect("leader exited successfully");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_task_kills_descendants_with_detached_output() {
        let marker_dir = tempfile::TempDir::new().unwrap();
        let marker = marker_dir.path().join("pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > \"{}\"; exit 0",
            marker.display()
        );

        let runner = SubprocessRunner::new("descendant-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("descendant-slot", "sh", &["-c", &script]);
        let task = build_with_run_id(&runner, &spec, &BuildContext::default()).unwrap();
        task.spawn(TaskContext::detached()).await.unwrap();

        let pid = wait_for_recorded_pid(&marker)
            .await
            .expect("descendant did not report its pid");
        assert_process_gone(pid).await;
    }

    #[test]
    fn resolve_mode_script_accepts_large_body() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let payload: Vec<u8> = b"# "
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b'x', 200 * 1024))
            .collect();
        let mode = solti_model::SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode(&payload),
            args: vec![],
        };
        let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES)
            .expect("200 KiB script must resolve via descriptor transport");
        assert_eq!(r.command, "bash");
        assert!(r.args.is_empty());
        let body = r
            .script_body
            .expect("large Script must carry the decoded body");
        assert_eq!(body.len(), payload.len());
    }

    #[test]
    fn resolve_mode_rejects_body_over_configured_limit() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mode = solti_model::SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode("a".repeat(100).as_bytes()),
            args: vec![],
        };
        let err = SubprocessRunner::resolve_mode(&mode, 10)
            .expect_err("body over the configured limit must be rejected");
        assert!(
            matches!(err, RunnerError::InvalidSpec(_)),
            "expected InvalidSpec, got {err:?}"
        );
    }
}
