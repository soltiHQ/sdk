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
//! output sink + cgroup + script file
//!      ▼
//! process group
//!   ├── stdout/stderr ──► tracing + output sink
//!   ├── exit ───────────► evaluate exit policy
//!   └── cancellation ───► kill process group
//!      ▼
//! remove script file and cgroup
//! ```
//!
//! On Unix, cancellation and future drop kill the complete process group.
//! On Unix, normal completion also stops descendants left by the leader.
//! Other platforms stop the leader process.

use std::{
    io::Write as _,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tempfile::NamedTempFile;
use tokio::process::Command;
use tracing::{debug, trace, warn};

use solti_model::{SubprocessSpec, Task, TaskWorkload, WORKLOAD_API_VERSION, WorkloadTypeMeta};
use solti_runner::{
    BuildContext, OutputPublisherHandle, RunId, Runner, RunnerError, RunnerErrorKind, RunnerType,
    merge_env,
};

use crate::subprocess::{
    backend::{PreparedSubprocessBackendConfig, SubprocessBackendConfig},
    logger::{LogConfig, StreamKind, log_stream},
    task::SubprocessTaskConfig,
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
/// | Permanent operating-system error       | Fatal failure                |
/// | Other operating-system error           | Retryable failure            |
///
/// On Unix, one attempt owns one process group.
/// The runner waits up to five seconds for output pipes after the leader exits.
/// It then stops remaining descendants.
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
    config: Option<Arc<PreparedSubprocessBackendConfig>>,
}

/// Validates a runner name before it is used in labels and paths.
///
/// The accepted syntax matches a Kubernetes label value.
fn validate_runner_name(name: &str) -> Result<(), crate::ExecError> {
    let edge_is_alphanumeric = name
        .as_bytes()
        .first()
        .zip(name.as_bytes().last())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    let ok = name.len() <= 63
        && edge_is_alphanumeric
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(crate::ExecError::InvalidRunnerConfig(format!(
            "invalid runner name {name:?}: must be a Kubernetes label value of 1..=63 ASCII characters"
        )))
    }
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
        Ok(Self { name, config: None })
    }

    /// Creates a subprocess runner with explicit backend settings.
    ///
    /// Configuration paths are resolved during this call.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExecError::InvalidRunnerConfig`] for invalid settings.
    /// Returns [`crate::ExecError::Io`] when current cgroup discovery fails.
    pub fn with_config(
        name: impl Into<String>,
        config: SubprocessBackendConfig,
    ) -> Result<Self, crate::ExecError> {
        let name = name.into();
        validate_runner_name(&name)?;
        let config = config.prepare()?;
        Ok(Self {
            name,
            config: Some(Arc::new(config)),
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
    ) -> Result<(SubprocessTaskConfig, Option<Arc<str>>), RunnerError> {
        let spec = task.spec();
        let (cfg, script_body) = match spec.workload() {
            TaskWorkload::Subprocess(SubprocessSpec {
                mode,
                env,
                cwd,
                fail_on_non_zero,
                ..
            }) => {
                let max_body = self
                    .config
                    .as_ref()
                    .map(|c| c.max_script_body_bytes())
                    .unwrap_or(solti_model::MAX_SCRIPT_BODY_BYTES);
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
        if let Some(backend) = self.config.as_ref() {
            backend
                .check_cwd(cfg.cwd.as_deref())
                .map_err(RunnerError::InvalidSpec)?;
        }
        Ok((cfg, script_body))
    }

    /// Resolves a subprocess mode into a command and arguments.
    ///
    /// Script bodies are decoded and checked here.
    /// File creation remains attempt-scoped.
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
    /// Script mode writes the body to a temporary file for each attempt.
    script_body: Option<Arc<str>>,
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
    /// It does not create a script file, cgroup, or process.
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
        let (task_cfg, script_body) = self.build_task_config(task, run_id, ctx)?;

        trace!(
            resource = %task.name(),
            generation = task.metadata().generation(),
            slot = %task.slot(),
            taskvisor_task = %task_cfg.run_id,
            "building subprocess task",
        );

        let cgroup_name = self.config.as_ref().and_then(|cfg| {
            cfg.has_cgroups().then(|| {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(StdDuration::from_secs(0))
                    .as_secs();
                build_cgroup_name(&self.name, task.slot().as_str(), task_cfg.seq, timestamp)
            })
        });

        let log_cfg = self
            .config
            .as_ref()
            .map(|c| *c.log_config())
            .unwrap_or_default();

        let exec_ctx = Arc::new(TaskExecContext {
            task_cfg,
            runner_cfg: self.config.clone(),
            cgroup_name,
            metrics: ctx.metrics().clone(),
            log_cfg,
            output_publisher: Arc::clone(ctx.output_publisher()),
            attempt: AtomicU32::new(0),
            generation: task.metadata().generation(),
            resource_name: task.name().clone(),

            script_body,
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
    runner_cfg: Option<Arc<PreparedSubprocessBackendConfig>>,
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
    /// Script mode materializes a fresh file for every attempt.
    script_body: Option<Arc<str>>,
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
    if let Some(cwd) = &ctx.task_cfg.cwd {
        cmd.current_dir(cwd);
    }
    apply_env_policy(&mut cmd, ctx);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Put the child into its own process group (pgid = child's pid).
    //
    // Why:
    // `child.kill()` delivers SIGKILL to the single pid only.
    // Scripts routinely fork background children (`sleep 100 &`, `nc -l &`, any daemonized helper);
    // without a process group those children survive the cancel and become zombies orphaned to PID 1.
    //
    // By spawning with `setpgid(0, 0)` via `process_group(0)` we ensure that `kill(-pgid, SIGKILL)` in the cancel path reaps the whole subtree at once.
    #[cfg(unix)]
    cmd.process_group(0);

    // Last-ditch safety:
    // if the `run_subprocess` future is dropped before reaching either the `wait` or the `kill` branch
    // (panic inside log streams, supervisor panic, future aborted by `tokio::select!` sibling),
    // tokio should kill the child on Drop rather than leaving it orphaned.
    cmd.kill_on_drop(true);

    cmd
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

    let default_policy = EnvPolicy::default();
    let policy = ctx
        .runner_cfg
        .as_ref()
        .map(|c| c.env_policy())
        .unwrap_or(&default_policy);

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

/// Kills an active Unix process group when the attempt future is dropped.
///
/// Taskvisor timeouts and forced aborts can drop the future before cooperative cancellation runs.
/// The guard remains armed until the child is reaped.
struct ProcessGroupGuard {
    /// Process group id while the guard is armed.
    pgid: Option<i32>,
    run_id: Arc<str>,
}

impl ProcessGroupGuard {
    fn new(pgid: Option<i32>, run_id: Arc<str>) -> Self {
        Self { pgid, run_id }
    }

    /// Disarms the guard after process-group cleanup.
    fn disarm(&mut self) {
        self.pgid = None;
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let rc = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    warn!(
                        task = %self.run_id,
                        error = %error,
                        "failed to kill subprocess group",
                    );
                }
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Kills the complete process group led by `child`.
///
/// Other platforms fall back to the leader process.
async fn kill_process_group(child: &mut tokio::process::Child, run_id: &str) {
    #[cfg(unix)]
    {
        // `child.id()` is `None` only once the child has already been reaped —
        // nothing to kill in that case.
        if let Some(pid) = child.id() {
            // SAFETY:
            // `libc::kill` has no memory preconditions.
            // The negative pid is the killpg idiom (targets the whole group, not a single process);
            // `pid` is the live child's id (group leader via `process_group(0)`).
            // On a non-ESRCH error we fall back to the single-pid `child.kill()`.
            let rc = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    warn!(
                        task = %run_id,
                        error = %err,
                        "killpg failed; falling back to single-pid kill",
                    );
                    let _ = child.kill().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
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

/// Creates a script temporary file and writes its body.
///
/// Unix files use mode `0600`.
fn write_script_tempfile(
    dir: &std::path::Path,
    body: &str,
) -> Result<NamedTempFile, std::io::Error> {
    let mut tmp = NamedTempFile::with_prefix_in("solti-script-", dir)
        .map_err(|e| io_with_context("failed to create script tempfile", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| io_with_context("failed to chmod 0600 script tempfile", e))?;
    }

    tmp.write_all(body.as_bytes())
        .map_err(|e| io_with_context("failed to write script body", e))?;

    Ok(tmp)
}

/// Writes one attempt's script file outside the async runtime.
///
/// A creation or write failure ends the attempt.
async fn materialize_script(
    ctx: &TaskExecContext,
    body: Arc<str>,
) -> Result<NamedTempFile, TaskError> {
    let written =
        tokio::task::spawn_blocking(move || write_script_tempfile(&std::env::temp_dir(), &body))
            .await;

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

/// Prepares the attempt cgroup outside the async runtime.
async fn prepare_backend(
    ctx: &TaskExecContext,
    cgroup_name: Option<String>,
) -> Result<Option<crate::host::PreparedHostProcessAttempt>, TaskError> {
    let Some(backend) = &ctx.runner_cfg else {
        return Ok(None);
    };
    if cgroup_name.is_none() {
        return backend
            .prepare_host_process_attempt(None)
            .map(Some)
            .map_err(|error| {
                ctx.metrics.record_runner_error(
                    RunnerType::Subprocess,
                    RunnerErrorKind::CgroupPrepareFailed,
                );
                TaskError::fatal(format!("host process resource preparation failed: {error}"))
            });
    }

    let backend = Arc::clone(backend);
    let prepared = tokio::task::spawn_blocking(move || {
        backend.prepare_host_process_attempt(cgroup_name.as_deref())
    })
    .await;
    match prepared {
        Ok(Ok(prepared)) => Ok(Some(prepared)),
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

/// Attaches rlimits, cgroup membership, and security controls.
fn apply_backend(
    cmd: &mut Command,
    ctx: &TaskExecContext,
    prepared: Option<crate::host::PreparedHostProcessAttempt>,
) -> Option<crate::host::HostProcessGuard> {
    if let Some(backend_cfg) = &ctx.runner_cfg {
        let prepared = prepared.expect("configured backend must have prepared host resources");
        Some(backend_cfg.apply_to_command(cmd, prepared))
    } else {
        debug_assert!(prepared.is_none());
        None
    }
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

/// Removes an attempt cgroup after completion or future drop.
struct CgroupGuard(Option<crate::host::HostProcessGuard>);

impl CgroupGuard {
    fn new(guard: Option<crate::host::HostProcessGuard>) -> Self {
        Self(guard)
    }

    async fn cleanup(mut self) {
        if let Some(mut guard) = self.0.take() {
            cleanup_host_process_guard(&mut guard).await;
        }
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        let Some(mut guard) = self.0.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                cleanup_host_process_guard(&mut guard).await;
            });
        } else {
            let _ = guard.cleanup();
        }
    }
}

async fn cleanup_host_process_guard(guard: &mut crate::host::HostProcessGuard) {
    let Some(path) = guard.cgroup_path().map(PathBuf::from) else {
        return;
    };
    const ATTEMPTS: usize = 10;
    for attempt in 0..ATTEMPTS {
        let current = path.clone();
        let result =
            tokio::task::spawn_blocking(move || crate::host::cleanup_cgroup(&current)).await;
        match result {
            Ok(Ok(())) => {
                let _ = guard.cleanup();
                return;
            }
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = guard.cleanup();
                return;
            }
            Ok(Err(error)) if attempt + 1 == ATTEMPTS => {
                warn!(cgroup = %path.display(), error = %error, "failed to remove cgroup");
                return;
            }
            Err(error) => {
                warn!(cgroup = %path.display(), error = %error, "cgroup cleanup worker failed");
                return;
            }
            Ok(Err(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(10 * (attempt as u64 + 1)))
                    .await;
            }
        }
    }
}

#[cfg(not(test))]
const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(100);

/// Executes one subprocess attempt.
async fn run_subprocess(ctx: Arc<TaskExecContext>, cancel: TaskContext) -> Result<(), TaskError> {
    let attempt = ctx.attempt.fetch_add(1, Ordering::Relaxed) + 1;
    let sink = ctx
        .output_publisher
        .sink_for(&ctx.resource_name, ctx.generation, attempt);

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

    // Script mode: write the body to a 0600 tempfile on a blocking thread.
    // The handle must outlive the child (the interpreter reads the file as it
    // executes); it is dropped — and the file unlinked — when this attempt ends.
    let script_tempfile = match &ctx.script_body {
        Some(body) => Some(materialize_script(&ctx, Arc::clone(body)).await?),
        None => None,
    };

    let mut cmd = build_command(&ctx, script_tempfile.as_ref().map(|t| t.path()));
    let host_process_guard = apply_backend(&mut cmd, &ctx, prepared_host_process);
    let cgroup_guard = CgroupGuard::new(host_process_guard);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
            return Err(task_io_error("spawn failed", e));
        }
    };
    let mut pg_guard = ProcessGroupGuard::new(
        child.id().map(|pid| pid as i32),
        Arc::clone(&ctx.task_cfg.run_id),
    );

    let log_cfg = ctx.log_cfg;

    let stdout = child
        .stdout
        .take()
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

    let stderr = child
        .stderr
        .take()
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

    let result = tokio::select! {
        biased;
        res = child.wait() => {
            match res {
                Ok(status) => evaluate_exit(status, &ctx.task_cfg),
                Err(error) => Err(task_io_error("wait failed", error)),
            }
        }
        _ = cancel.cancelled() => {
            debug!(
                task = %ctx.task_cfg.run_id,
                "cancellation requested; killing subprocess group",
            );
            kill_process_group(&mut child, &ctx.task_cfg.run_id).await;
            let _ = child.wait().await;
            Err(TaskError::Canceled)
        }
    };

    let drained = tokio::time::timeout(LOG_DRAIN_GRACE, async {
        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
    })
    .await;
    if drained.is_err() {
        pg_guard.kill();
        stdout_task.abort();
        stderr_task.abort();
        warn!(
            task = %ctx.task_cfg.run_id,
            "subprocess output drain timed out after leader exit; killed lingering process group",
        );
    }
    // The task owns its entire process group. Descendants must not outlive the
    // leader even when they detached their standard streams.
    pg_guard.kill();
    pg_guard.disarm();
    cgroup_guard.cleanup().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_name_is_stable() {
        assert_eq!(
            build_cgroup_name("runner", "slot", 42, 1000),
            "runner-slot-2a-3e8"
        );
    }

    #[tokio::test]
    async fn cgroup_guard_explicit_cleanup_removes_empty_group() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let host = crate::host::HostProcessGuard::for_test(path.clone());

        CgroupGuard::new(Some(host)).cleanup().await;

        assert!(!path.exists());
    }

    #[test]
    fn cgroup_guard_drop_removes_empty_group_without_runtime() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let host = crate::host::HostProcessGuard::for_test(path.clone());

        drop(CgroupGuard::new(Some(host)));

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cgroup_guard_drop_schedules_cleanup_inside_runtime() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let host = crate::host::HostProcessGuard::for_test(path.clone());

        drop(CgroupGuard::new(Some(host)));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cgroup cleanup task did not finish");
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
            runner_cfg: None,
            cgroup_name: None,
            metrics: solti_runner::noop_metrics(),
            log_cfg: LogConfig::default(),
            output_publisher: solti_runner::noop_output_publisher(),
            attempt: AtomicU32::new(0),
            generation: 1,
            resource_name: solti_model::TaskId::new("test-resource").unwrap(),
            script_body: None,
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
        ctx.runner_cfg = Some(Arc::new(cfg.prepare().unwrap()));
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
            "script output must reach the registry (tempfile materialized at run time, extra args preserved)"
        );
    }

    #[tokio::test]
    async fn script_task_can_be_spawned_repeatedly() {
        // The tempfile is materialized per attempt; a retry after the first
        // attempt (and its tempfile) is gone must still find its script.
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
    fn resolve_mode_script_defers_tempfile_to_run_time() {
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
            "resolve must not touch the disk: the tempfile path is prepended at spawn time"
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

    #[test]
    fn write_script_tempfile_writes_body_with_0600() {
        let tmp = write_script_tempfile(&std::env::temp_dir(), "echo hello")
            .expect("tempfile must be written");
        let written = std::fs::read_to_string(tmp.path()).expect("tempfile readable");
        assert_eq!(written, "echo hello");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(tmp.path()).unwrap().permissions();
            assert_eq!(
                perms.mode() & 0o777,
                0o600,
                "tempfile must be chmod 0600 (may carry secrets)"
            );
        }
    }

    #[test]
    fn write_script_tempfile_fails_loudly_on_bad_dir() {
        let parent = tempfile::TempDir::new().unwrap();
        let bogus = parent.path().join("missing");
        let err = write_script_tempfile(&bogus, "echo hello")
            .expect_err("nonexistent dir must fail tempfile creation");
        assert!(
            err.to_string().contains("failed to create script tempfile"),
            "got: {err}"
        );
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
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().expect("bash must spawn");
        let grandchild_pid = wait_for_recorded_pid(&marker).await;
        if let Some(pid) = grandchild_pid {
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "grandchild must be alive before cancel"
            );
        }

        kill_process_group(&mut child, "test").await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        let grandchild_pid = grandchild_pid.expect("grandchild did not report its pid");
        assert_process_gone(grandchild_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_group() {
        // taskvisor enforces a per-attempt timeout via `tokio::time::timeout` and
        // force-abort via `JoinHandle::abort` — both DROP the task future without ever
        // polling the cooperative `cancel.cancelled()` branch. `kill_on_drop(true)`
        // only SIGKILLs the leader pid; forked grandchildren would be orphaned.
        // The subtree must still be reaped on drop.
        let marker_dir = tempfile::TempDir::new().unwrap();
        let marker = marker_dir.path().join("pid");
        let marker_str = marker.to_string_lossy().to_string();

        // Fork a long-lived grandchild, record its pid, then block forever.
        let script = format!(r#"(sleep 60 & echo $! > "{marker_str}") ; sleep 60"#);

        let runner = SubprocessRunner::new("test-runner").unwrap();
        let spec = mk_subprocess_spec_with_args("drop-slot", "bash", &["-c", &script]);
        let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default()).unwrap();

        let cancel = TaskContext::detached();
        let handle = tokio::spawn(async move { task_ref.spawn(cancel).await });
        let grandchild_pid = wait_for_recorded_pid(&marker).await;

        handle.abort();
        let _ = handle.await;
        let grandchild_pid = grandchild_pid.expect("grandchild did not report its pid");
        assert_process_gone(grandchild_pid).await;
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
            .expect("200 KiB script must resolve via tempfile transport");
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
