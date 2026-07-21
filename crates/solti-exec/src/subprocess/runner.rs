//! # Runner: subprocess execution engine.
//!
//! [`SubprocessRunner`] implements the [`Runner`](solti_runner::Runner) trait to execute
//! [`TaskWorkload::Subprocess`](solti_model::TaskWorkload::Subprocess) tasks as OS processes.
//!
//! ## How it works
//! ```text
//! SubprocessRunner::build_task(task, ctx)
//!     │
//!     ├──► build_task_config(task, ctx)
//!     │     ├──► match TaskWorkload::Subprocess(SubprocessSpec { .. })
//!     │     ├──► resolve_mode(mode) → Resolved { command, args, script_body? }
//!     │     │     ├──► Command { command, args } → clone, script_body = None
//!     │     │     └──► Script { runtime, body }  → decode base64 (size-checked)
//!     │     │                                    → script_body = Some(body)
//!     │     ├──► merge_env(task_env, runner_env)→ BTreeMap
//!     │     └──► SubprocessTaskConfig { run_id, seq, command, args, env, cwd, fail_on_non_zero }
//!     │
//!     ├──► prepare backend (cgroup dirs, if configured)
//!     │
//!     ├──► build Arc<TaskExecContext>
//!     │     └──► { task_cfg, runner_cfg, cgroup_name, metrics, log_cfg, script_body? }
//!     │          no disk I/O on the submit path: the tempfile is written per attempt
//!     │
//!     └──► return TaskFn closure → run_subprocess(ctx, cancel)
//! ```
//!
//! ## Subprocess execution lifecycle
//! ```text
//! run_subprocess(ctx, cancel)
//!     │
//!     ├──► allocate attempt + OutputSink
//!     ├──► metrics.record_task_started()
//!     ├──► prepare_backend() → create cgroup dirs
//!     ├──► materialize_script() (Script mode only)
//!     │      - spawn_blocking: NamedTempFile (0600) + write + fsync
//!     │      - tempfile path is prepended to args
//!     │      - lives until the attempt ends → Drop = unlink
//!     ├──► build_command() → Command with:
//!     │      - args, env, cwd, piped stdout/stderr
//!     │      - process_group(0)   (Unix: new pgid for kill-whole-subtree)
//!     │      - kill_on_drop(true) (tokio SIGKILLs PID if future is dropped)
//!     ├──► apply_backend() → install pre_exec hooks (rlimits, cgroup join, security)
//!     ├──► cmd.spawn()
//!     ├──► arm ProcessGroupGuard(pgid)  (Drop = killpg -SIGKILL pgid if the future is dropped)
//!     │
//!     ├──► tokio::spawn(log_stream(stdout, Stdout))
//!     ├──► tokio::spawn(log_stream(stderr, Stderr))
//!     │
//!     ├──► select! { biased; ... }
//!     │     ├──► child.wait() → evaluate_exit(status)
//!     │     └──► cancel.cancelled() → kill_process_group() (killpg -SIGKILL pgid)
//!     │                             → child.wait() to reap zombie
//!     ├──► guard.disarm()  (child reaped; drop no longer kills)
//!     │
//!     ├──► metrics.record_task_completed(outcome, duration)
//!     ├──► join!(stdout_task, stderr_task)
//!     ├──► cleanup_cgroup() (if configured)
//!     └──► return result
//! ```

use std::{
    io::Write as _,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tempfile::NamedTempFile;
use tokio::process::Command;
use tracing::{debug, trace, warn};

use solti_model::{Runtime, SubprocessSpec, Task, TaskWorkload};
use solti_runner::{
    BuildContext, OutputPublisherHandle, Runner, RunnerError, RunnerErrorKind, RunnerType,
    merge_env,
};

use crate::metrics::classify_task_error;
use crate::subprocess::{
    backend::SubprocessBackendConfig,
    logger::{LogConfig, StreamKind, log_stream},
    task::SubprocessTaskConfig,
};

/// Runner that executes [`TaskWorkload::Subprocess`] as OS subprocesses.
///
/// ## Also
///
/// - [`SubprocessBackendConfig`] rlimits, cgroups, security applied to spawned processes.
/// - [`register_subprocess_runner`](super::register_subprocess_runner) registration helper.
/// - [`solti_runner::Runner`] trait this type implements.
pub struct SubprocessRunner {
    /// Runner name.
    name: &'static str,
    /// Backend configuration applied to all tasks spawned by this runner.
    config: Option<Arc<SubprocessBackendConfig>>,
}

/// Validate a runner name before it is embedded into run IDs and cgroup paths.
///
/// The name reaches the filesystem via `build_cgroup_name`, so it must not carry
/// path separators, `.`/`..`, or non-portable characters. The rule matches the
/// model's identity charset: non-empty, ASCII alphanumeric plus `-`, `_`, `.`,
/// and no lone `.`/`..`, capped at 64 bytes.
fn validate_runner_name(name: &str) -> Result<(), crate::ExecError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(crate::ExecError::InvalidRunnerConfig(format!(
            "invalid runner name {name:?}: must be 1..=64 chars of [A-Za-z0-9._-] and not '.'/'..'"
        )))
    }
}

impl SubprocessRunner {
    /// Create a new subprocess runner without backend configuration.
    ///
    /// `name` must be a valid runner identity (see [`with_config`](Self::with_config)
    /// for the rule); it is embedded into run IDs and cgroup paths. Because this
    /// constructor is infallible, an invalid name is a debug assertion here and
    /// is rejected outright by [`with_config`](Self::with_config).
    pub fn new(name: &'static str) -> Self {
        debug_assert!(
            validate_runner_name(name).is_ok(),
            "invalid runner name {name:?}",
        );
        Self { name, config: None }
    }

    /// Create a subprocess runner with explicit backend configuration.
    ///
    /// ## Errors
    ///
    /// - [`ExecError::InvalidRunnerConfig`](crate::ExecError::InvalidRunnerConfig): the
    ///   `name` is not a valid runner identity (non-empty, `[A-Za-z0-9._-]`, ≤ 64 bytes,
    ///   not `.`/`..`), or `config` failed validation. Config examples: a zero
    ///   cgroup/rlimit/log limit, `keep_caps` without `drop_all_caps`, or
    ///   `require_enforcement` on a non-Linux host.
    pub fn with_config(
        name: &'static str,
        config: SubprocessBackendConfig,
    ) -> Result<Self, crate::ExecError> {
        validate_runner_name(name)?;
        config.validate()?;
        Ok(Self {
            name,
            config: Some(Arc::new(config)),
        })
    }

    /// Build task configuration from a [`Task`] resource.
    ///
    /// Returns the fully resolved config.
    fn build_task_config(
        &self,
        task: &Task,
        ctx: &BuildContext,
    ) -> Result<(SubprocessTaskConfig, Option<Arc<str>>), RunnerError> {
        let spec = task.spec();
        let slot = spec.slot();
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
                let run_id = self.build_run_id(slot.as_str());
                let cfg = SubprocessTaskConfig {
                    seq: run_id.seq(),
                    run_id: Arc::from(run_id.into_name()),
                    fail_on_non_zero: *fail_on_non_zero,
                    env: merge_env(env, ctx.env()),
                    cwd: cwd.clone(),
                    command,
                    args,
                };
                (cfg, script_body)
            }
            other => {
                return Err(RunnerError::UnsupportedKind {
                    runner: self.name,
                    kind: format!("{}/{}", other.api_version(), other.kind()),
                });
            }
        };
        cfg.validate()
            .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;
        if let Some(backend) = self.config.as_ref() {
            backend
                .check_cwd(cfg.cwd.as_deref())
                .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;
        }
        Ok((cfg, script_body))
    }

    /// Resolve [`SubprocessMode`](solti_model::SubprocessMode) into a command + args pair ready for `execve`.
    ///
    /// ## Script transport
    ///
    /// For `Script` mode the body is decoded (and size-checked) here, but the
    /// tempfile is **not** written yet: `build_task` runs on the async submit
    /// path, so the disk I/O (create + write + fsync) is deferred to
    /// [`materialize_script`] inside the task body, where it runs on a blocking
    /// thread per attempt. The interpreter is then invoked with the tempfile
    /// path: *not* with `-c "<inline>"`.
    ///
    /// ## Limits
    ///
    /// - The inline form is limited to `MAX_ARG_STRLEN` (128 KiB on Linux);
    /// - The tempfile form supports scripts up to [`MAX_SCRIPT_BODY_BYTES`](solti_model::MAX_SCRIPT_BODY_BYTES) (2 MiB).
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
            solti_model::SubprocessMode::Script { runtime, args, .. } => {
                let script = mode
                    .decode_body_with_limit(max_script_body_bytes)
                    .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;

                let cmd = resolve_runtime_command(runtime)?;

                Ok(Resolved {
                    command: cmd.to_owned(),
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

/// Output of [`SubprocessRunner::resolve_mode`].
#[derive(Debug)]
struct Resolved {
    command: String,
    args: Vec<String>,

    /// Decoded script body for `Script` mode; `None` for `Command`.
    ///
    /// Written to a tempfile by [`materialize_script`] when the task runs;
    /// the tempfile path is prepended to `args` at spawn time.
    script_body: Option<Arc<str>>,
}

/// Resolve model data into the executable selected by this backend.
///
/// The model owns only the serializable runtime choice. Executable names are a
/// subprocess-backend policy and therefore live in `solti-exec`.
fn resolve_runtime_command(runtime: &Runtime) -> Result<&str, RunnerError> {
    match runtime {
        Runtime::Bash => Ok("bash"),
        Runtime::Python => Ok("python3"),
        Runtime::Node => Ok("node"),
        Runtime::Custom { command, .. } => Ok(command),
        _ => Err(RunnerError::InvalidSpec(
            "unsupported script runtime variant".into(),
        )),
    }
}

impl Runner for SubprocessRunner {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, workload: &TaskWorkload) -> bool {
        matches!(workload, TaskWorkload::Subprocess(_))
    }

    /// Turn a [`TaskWorkload::Subprocess`] resource into a runnable [`TaskRef`].
    ///
    /// Resolves the subprocess mode, merges the environment, and captures the
    /// resolved config in a closure that spawns the OS process when the task runs.
    ///
    /// ## Errors
    ///
    /// - [`RunnerError::UnsupportedKind`]: the task workload is not [`TaskWorkload::Subprocess`].
    /// - [`RunnerError::InvalidSpec`]: the command is empty, or the script body is
    ///   not valid base64 or exceeds the configured size limit.
    ///
    /// The script tempfile is written inside the task body (per attempt); an I/O
    /// failure there fails the run with a fatal `TaskError`, not a build error.
    /// The output sink is also acquired inside each attempt from the producer
    /// capability injected through [`BuildContext`].
    fn build_task(&self, task: &Task, ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let (task_cfg, script_body) = self.build_task_config(task, ctx)?;

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
                crate::utils::build_cgroup_name(
                    self.name,
                    task.slot().as_str(),
                    task_cfg.seq,
                    timestamp,
                )
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

/// Shared context for subprocess task execution.
struct TaskExecContext {
    runner_cfg: Option<Arc<SubprocessBackendConfig>>,
    metrics: solti_runner::MetricsHandle,
    output_publisher: OutputPublisherHandle,
    task_cfg: SubprocessTaskConfig,
    cgroup_name: Option<String>,
    log_cfg: LogConfig,
    attempt: AtomicU32,
    generation: u64,
    resource_name: solti_model::TaskId,

    /// Decoded script body for `Script` mode; `None` for `Command`.
    ///
    /// Materialized into a fresh 0600 tempfile on every attempt by
    /// [`materialize_script`]; the tempfile is unlinked when the attempt ends.
    script_body: Option<Arc<str>>,
}

/// Build the OS command from task configuration.
///
/// `script_path` is the materialized script tempfile for `Script` mode; it is
/// prepended to the task args so the interpreter receives it as its first
/// argument. `None` for `Command` mode.
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

/// Build the child's environment per the backend [`EnvPolicy`].
///
/// The task's own vars are always applied last, so they win over inherited or
/// allowlisted values. Under `Clear`/`Allowlist` a [safe `PATH`] is injected
/// when the task set none, so a bare command name still resolves.
///
/// [safe `PATH`]: crate::subprocess::backend::SAFE_DEFAULT_PATH
fn apply_env_policy(cmd: &mut Command, ctx: &TaskExecContext) {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};

    let policy = ctx
        .runner_cfg
        .as_ref()
        .map(|c| c.effective_env_policy())
        .unwrap_or_default();

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
            for key in &keys {
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

/// Drop-safe reaper for the child's process group.
///
/// taskvisor enforces the per-attempt timeout via `tokio::time::timeout` and force-abort
/// via `JoinHandle::abort`; **both drop the `run_subprocess` future** without ever polling
/// the cooperative `cancel.cancelled()` branch. `kill_on_drop(true)` only SIGKILLs the
/// leader pid, leaving any forked grandchildren (the process group) orphaned to PID 1.
///
/// This guard captures the child's pgid right after spawn and, on `Drop`, sends
/// `kill(-pgid, SIGKILL)` to the whole group. It is [`disarm`](Self::disarm)ed once the
/// child has been reaped on a normal/explicit-kill path. It therefore never targets a
/// recycled pgid — it fires **only** when the future is dropped mid-flight.
struct ProcessGroupGuard {
    /// `Some(pgid)` while armed; `None` once the group is reaped. On Unix `pgid == child pid`
    /// because the child is spawned with `process_group(0)`.
    pgid: Option<i32>,
    run_id: Arc<str>,
}

impl ProcessGroupGuard {
    fn new(pgid: Option<i32>, run_id: Arc<str>) -> Self {
        Self { pgid, run_id }
    }

    /// Disarm after the child has been waited on (group already reaped).
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let rc = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    warn!(
                        task = %self.run_id,
                        error = %err,
                        "killpg on drop failed; subtree may be orphaned",
                    );
                }
            }
        }
    }
}

/// Kill of the entire process group led by `child`.
///
/// On Unix: `killpg(pid, SIGKILL)` via `libc::kill(-pid, SIGKILL)`
///
/// On other platforms: falls back to `child.kill()` (single PID only).
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

/// Create the script tempfile: 0600 permissions, body written and flushed to disk.
///
/// Runs on a blocking thread (see [`materialize_script`]): `sync_all` can park
/// the calling thread for tens of milliseconds and must never run on a tokio
/// worker. The returned handle unlinks the file on drop.
fn write_script_tempfile(dir: &std::path::Path, body: &str) -> Result<NamedTempFile, String> {
    let mut tmp = NamedTempFile::with_prefix_in("solti-script-", dir)
        .map_err(|e| format!("failed to create script tempfile: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| format!("failed to chmod 0600 script tempfile: {e}"))?;
    }

    tmp.write_all(body.as_bytes())
        .map_err(|e| format!("failed to write script body: {e}"))?;
    tmp.as_file()
        .sync_all()
        .or_else(|_| tmp.as_file().flush())
        .map_err(|e| format!("failed to flush script tempfile: {e}"))?;

    Ok(tmp)
}

/// Materialize the decoded script body into a tempfile, off the async runtime.
///
/// Called once per attempt from [`run_subprocess`]; the disk I/O (create +
/// write + fsync, bodies up to 2 MiB) runs via `spawn_blocking` so the submit
/// path and sibling tasks on the same worker are never stalled. A failure is
/// fatal for the attempt, mirroring the spawn-failure path.
async fn materialize_script(
    ctx: &TaskExecContext,
    body: Arc<str>,
) -> Result<NamedTempFile, TaskError> {
    let written =
        tokio::task::spawn_blocking(move || write_script_tempfile(&std::env::temp_dir(), &body))
            .await
            .map_err(|e| format!("script tempfile write task failed: {e}"))
            .and_then(|res| res);

    written.map_err(|e| {
        ctx.metrics
            .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        TaskError::fatal(e)
    })
}

/// Prepare backend resources (cgroup directories) before spawn.
fn prepare_backend(ctx: &TaskExecContext) -> Result<(), TaskError> {
    if let Some(backend_cfg) = &ctx.runner_cfg {
        let cgroup_name_ref = ctx.cgroup_name.as_deref().unwrap_or(&ctx.task_cfg.run_id);

        if let Err(e) = backend_cfg.prepare_cgroups(cgroup_name_ref) {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::CgroupPrepareFailed);
            return Err(TaskError::fatal(format!("failed to prepare cgroup: {e}")));
        }
    }
    Ok(())
}

/// Apply backend configuration (rlimits, cgroup join, security) to the command.
fn apply_backend(cmd: &mut Command, ctx: &TaskExecContext) -> Result<(), TaskError> {
    if let Some(backend_cfg) = &ctx.runner_cfg {
        let cgroup_name_ref = ctx.cgroup_name.as_deref().unwrap_or(&ctx.task_cfg.run_id);

        if let Err(e) = backend_cfg.apply_to_command(cmd, cgroup_name_ref) {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::BackendConfigFailed);
            return Err(TaskError::fatal(format!(
                "failed to apply runner config: {e}"
            )));
        }
    }
    Ok(())
}

/// Evaluate subprocess exit status.
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

/// RAII guard that calls `cleanup_cgroup` on drop.
struct CgroupGuard<'a>(Option<&'a str>);

impl Drop for CgroupGuard<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.0 {
            crate::utils::cleanup_cgroup(name);
        }
    }
}

const LOG_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Execute a subprocess task with cancellation support, metrics, and cleanup.
async fn run_subprocess(ctx: Arc<TaskExecContext>, cancel: TaskContext) -> Result<(), TaskError> {
    let start = Instant::now();
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

    prepare_backend(&ctx)?;

    let _cgroup_guard = CgroupGuard(ctx.cgroup_name.as_deref());

    // Script mode: write the body to a 0600 tempfile on a blocking thread.
    // The handle must outlive the child (the interpreter reads the file as it
    // executes); it is dropped — and the file unlinked — when this attempt ends.
    let script_tempfile = match &ctx.script_body {
        Some(body) => Some(materialize_script(&ctx, Arc::clone(body)).await?),
        None => None,
    };

    let mut cmd = build_command(&ctx, script_tempfile.as_ref().map(|t| t.path()));
    apply_backend(&mut cmd, &ctx)?;

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
            return Err(TaskError::fatal(format!("spawn failed: {e}")));
        }
    };
    ctx.metrics.record_task_started(RunnerType::Subprocess);

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
            let status = res.map_err(|e| TaskError::fatal(format!("wait failed: {e}")))?;
            evaluate_exit(status, &ctx.task_cfg)
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

    #[cfg(unix)]
    let pgid = pg_guard.pgid;

    pg_guard.disarm();

    let duration_ms = start.elapsed().as_millis() as u64;
    let outcome = match &result {
        Ok(()) => solti_runner::MetricOutcome::Success,
        Err(e) => classify_task_error(e),
    };
    ctx.metrics
        .record_task_completed(RunnerType::Subprocess, outcome, duration_ms);

    let drained = tokio::time::timeout(LOG_DRAIN_GRACE, async {
        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
    })
    .await;
    if drained.is_err() {
        #[cfg(unix)]
        if let Some(pgid) = pgid {
            // SAFETY:
            // `libc::kill` has no memory preconditions.
            // The negative pid is the killpg idiom (targets the whole group, not a single pid);
            // `pgid` was captured at spawn from `process_group(0)`, and the leader is already reaped, this reaches only still-living grandchildren.
            // `ESRCH` (group already gone) is benign.
            let rc = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    warn!(
                        task = %ctx.task_cfg.run_id,
                        error = %err,
                        "killpg of lingering subtree failed",
                    );
                }
            }
        }
        stdout_task.abort();
        stderr_task.abort();
        warn!(
            task = %ctx.task_cfg.run_id,
            "subprocess output drain timed out after leader exit; killed lingering process group",
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    runtime: solti_model::Runtime::Bash,
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
            resource_name: "test-resource".into(),
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
    fn build_command_prepends_script_path() {
        let ctx = make_exec_ctx();
        let cmd = build_command(&ctx, Some(std::path::Path::new("/tmp/solti-script-x")));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(args, vec!["/tmp/solti-script-x", "hello"]);
    }

    #[test]
    fn build_command_sets_env() {
        let mut ctx = make_exec_ctx();
        ctx.task_cfg.env.insert("FOO".into(), "bar".into());
        let cmd = build_command(&ctx, None);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "FOO" && *v == Some(std::ffi::OsStr::new("bar")))
        );
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
        ctx.runner_cfg = Some(Arc::new(cfg));
        ctx
    }

    #[test]
    fn env_inherit_injects_no_path() {
        // Default (Inherit): no env_clear, no synthetic PATH — the child inherits
        // the agent's PATH as before.
        let ctx = ctx_with_backend(SubprocessBackendConfig::new());
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
    fn evaluate_exit_success() {
        use std::process::Command as StdCommand;
        let status = StdCommand::new("true").status().unwrap();
        let cfg = make_task_cfg();
        assert!(evaluate_exit(status, &cfg).is_ok());
    }

    #[test]
    fn evaluate_exit_non_zero_with_fail_flag() {
        use std::process::Command as StdCommand;
        let status = StdCommand::new("false").status().unwrap();
        let mut cfg = make_task_cfg();
        cfg.fail_on_non_zero = solti_model::Flag::enabled();
        let result = evaluate_exit(status, &cfg);
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
    fn evaluate_exit_non_zero_without_fail_flag() {
        use std::process::Command as StdCommand;
        let status = StdCommand::new("false").status().unwrap();
        let mut cfg = make_task_cfg();
        cfg.fail_on_non_zero = solti_model::Flag::disabled();
        assert!(evaluate_exit(status, &cfg).is_ok());
    }

    #[test]
    fn build_task_returns_task_ref_for_subprocess() {
        let runner = SubprocessRunner::new("test-runner");
        let task = mk_subprocess_spec("test-slot", "echo");
        let task_ref = runner.build_task(&task, &BuildContext::default()).unwrap();

        assert_ne!(task_ref.name(), task.name().as_str());
        assert!(task_ref.name().starts_with("test-runner-test-slot-"));
    }

    #[test]
    fn build_task_rejects_non_subprocess_kind() {
        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_embedded_spec("test-slot");
        match runner.build_task(&spec, &BuildContext::default()) {
            Err(RunnerError::UnsupportedKind { runner, kind }) => {
                assert_eq!(runner, "test-runner");
                assert_eq!(kind, "solti.io/v1/Embedded");
            }
            Err(other) => panic!("expected UnsupportedKind, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn supports_returns_true_for_subprocess() {
        let runner = SubprocessRunner::new("test");
        let task = mk_subprocess_spec("s", "echo");
        assert!(runner.supports(task.spec().workload()));
    }

    #[test]
    fn runtime_executable_resolution_belongs_to_exec_backend() {
        assert_eq!(resolve_runtime_command(&Runtime::Bash).unwrap(), "bash");
        assert_eq!(
            resolve_runtime_command(&Runtime::Python).unwrap(),
            "python3"
        );
        assert_eq!(resolve_runtime_command(&Runtime::Node).unwrap(), "node");
        assert_eq!(
            resolve_runtime_command(&Runtime::Custom {
                command: "ruby".into(),
                flag: "-e".into(),
            })
            .unwrap(),
            "ruby"
        );
    }

    #[test]
    fn supports_returns_false_for_embedded() {
        let runner = SubprocessRunner::new("test");
        let task = mk_embedded_spec("s");
        assert!(!runner.supports(task.spec().workload()));
    }

    #[test]
    fn build_task_returns_task_ref_for_script_mode() {
        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_script_spec("test-slot", b"echo hello", &[]);
        let result = runner.build_task(&spec, &BuildContext::default());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn script_task_runs_and_streams_output() {
        use solti_model::OutputEvent;
        use std::time::Duration;

        let (ctx, rx, _calls) = recording_output_context();

        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_script_spec("script-e2e", b"echo \"hello-$1\"", &["script"]);
        let task_ref = runner.build_task(&spec, &ctx).unwrap();
        let cancel = TaskContext::detached();
        task_ref
            .spawn(cancel)
            .await
            .expect("script task must succeed");

        let mut found = false;
        for _ in 0..100 {
            if let Ok(OutputEvent::Chunk(c)) = rx.try_recv() {
                if std::str::from_utf8(&c.line)
                    .unwrap_or_default()
                    .contains("hello-script")
                {
                    found = true;
                    break;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        assert!(
            found,
            "script output must reach the registry (tempfile materialized at run time, extra args preserved)"
        );
    }

    #[tokio::test]
    async fn script_task_can_be_spawned_repeatedly() {
        // The tempfile is materialized per attempt; a retry after the first
        // attempt (and its tempfile) is gone must still find its script.
        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_script_spec("script-retry", b"exit 0", &[]);
        let task_ref = runner.build_task(&spec, &BuildContext::default()).unwrap();

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
            runtime: solti_model::Runtime::Bash,
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
    fn resolve_mode_script_custom_ignores_flag() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mode = solti_model::SubprocessMode::Script {
            runtime: solti_model::Runtime::Custom {
                command: "ruby".into(),
                flag: "-e".into(),
            },
            body: BASE64.encode(b"puts 'hi'"),
            args: vec![],
        };
        let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
        assert_eq!(r.command, "ruby");
        assert!(r.args.is_empty(), "flag must not leak into args");
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
        let bogus = std::env::temp_dir().join("solti-definitely-missing-dir-xyz");
        let err = write_script_tempfile(&bogus, "echo hello")
            .expect_err("nonexistent dir must fail tempfile creation");
        assert!(
            err.contains("failed to create script tempfile"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn materialize_script_writes_tempfile_off_the_runtime() {
        let ctx = make_exec_ctx();
        let tmp = materialize_script(&ctx, Arc::from("echo hi"))
            .await
            .expect("materialization must succeed");
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(written, "echo hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_reaps_forked_grandchildren() {
        use std::process::Stdio;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;
        use tokio::process::Command as TokioCommand;
        use tokio::time::timeout;

        static N: AtomicU32 = AtomicU32::new(0);
        let marker = std::env::temp_dir().join(format!(
            "solti-exec-pgid-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let marker_str = marker.to_string_lossy().to_string();

        let script = format!(
            r#"
            (sleep 60 & echo $! > {marker}) &
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

        let grandchild_pid: i32 = {
            let mut attempts = 0;
            loop {
                if let Ok(s) = std::fs::read_to_string(&marker)
                    && let Some(line) = s.trim().lines().next()
                    && let Ok(pid) = line.parse::<i32>()
                {
                    break pid;
                }
                attempts += 1;
                if attempts > 50 {
                    panic!("grandchild never reported its pid via marker");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };

        let alive = unsafe { libc::kill(grandchild_pid, 0) };
        assert_eq!(alive, 0, "grandchild must be alive before cancel");

        kill_process_group(&mut child, "test").await;
        let _ = timeout(Duration::from_secs(2), child.wait()).await;

        let mut caught = false;
        for _ in 0..50 {
            let rc = unsafe { libc::kill(grandchild_pid, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                caught = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = std::fs::remove_file(&marker);

        if !caught {
            unsafe { libc::kill(grandchild_pid, libc::SIGKILL) };
            panic!(
                "grandchild PID {} survived cancel — process-group kill did not reach it",
                grandchild_pid
            );
        }
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_group() {
        // taskvisor enforces a per-attempt timeout via `tokio::time::timeout` and
        // force-abort via `JoinHandle::abort` — both DROP the task future without ever
        // polling the cooperative `cancel.cancelled()` branch. `kill_on_drop(true)`
        // only SIGKILLs the leader pid; forked grandchildren would be orphaned.
        // The subtree must still be reaped on drop.
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Duration;
        use tokio::time::timeout;

        static N: AtomicU32 = AtomicU32::new(0);
        let marker = std::env::temp_dir().join(format!(
            "solti-exec-droppgid-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let marker_str = marker.to_string_lossy().to_string();

        // Fork a long-lived grandchild, record its pid, then block forever.
        let script = format!(r#"(sleep 60 & echo $! > {marker_str}) ; sleep 60"#);

        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_subprocess_spec_with_args("drop-slot", "bash", &["-c", &script]);
        let task_ref = runner.build_task(&spec, &BuildContext::default()).unwrap();

        // Run, then DROP the future via timeout — exactly what taskvisor does.
        let cancel = TaskContext::detached();
        let _ = timeout(Duration::from_millis(500), task_ref.spawn(cancel)).await;

        let grandchild_pid: i32 = {
            let mut attempts = 0;
            loop {
                if let Ok(s) = std::fs::read_to_string(&marker)
                    && let Some(line) = s.trim().lines().next()
                    && let Ok(pid) = line.parse::<i32>()
                {
                    break pid;
                }
                attempts += 1;
                if attempts > 50 {
                    panic!("grandchild never reported its pid via marker");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };

        let mut caught = false;
        for _ in 0..50 {
            let rc = unsafe { libc::kill(grandchild_pid, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                caught = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = std::fs::remove_file(&marker);

        if !caught {
            unsafe { libc::kill(grandchild_pid, libc::SIGKILL) };
            panic!(
                "grandchild PID {grandchild_pid} survived the dropped future — the process subtree was orphaned"
            );
        }
    }

    #[test]
    fn resolve_mode_invalid_base64() {
        let mode = solti_model::SubprocessMode::Script {
            runtime: solti_model::Runtime::Bash,
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
        use std::time::Duration;

        let (ctx, rx, calls) = recording_output_context();

        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_subprocess_spec_with_args("echo-slot", "echo", &["hello-stream"]);
        let task_ref = runner.build_task(&spec, &ctx).unwrap();
        let cancel = TaskContext::detached();
        task_ref.spawn(cancel).await.expect("echo must succeed");

        let mut found_line = None;
        for _ in 0..100 {
            if let Ok(OutputEvent::Chunk(c)) = rx.try_recv() {
                let line_text = std::str::from_utf8(&c.line).unwrap_or_default();
                if line_text.contains("hello-stream") {
                    found_line = Some(c);
                    break;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        let chunk = found_line.expect("expected to receive 'hello-stream' line");
        assert_eq!(chunk.attempt, 1);
        assert_eq!(chunk.generation, 1);
        assert_eq!(chunk.stream, solti_model::StreamKind::Stdout);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.as_slice(), &[("task-echo-slot".into(), 1, 1)]);
    }

    #[tokio::test]
    async fn subprocess_attempt_counter_increments_on_each_spawn() {
        use solti_model::OutputEvent;
        use std::time::Duration;

        let (ctx, rx, _calls) = recording_output_context();
        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_subprocess_spec_with_args("attempts-slot", "echo", &["x"]);
        let task_ref = runner.build_task(&spec, &ctx).unwrap();
        let ctx = TaskContext::detached();
        task_ref.spawn(ctx.clone()).await.unwrap();
        task_ref.spawn(ctx).await.unwrap();

        let mut attempts = std::collections::BTreeSet::new();
        for _ in 0..200 {
            match rx.try_recv() {
                Ok(OutputEvent::Chunk(c)) => {
                    attempts.insert(c.attempt);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        assert!(attempts.contains(&1), "attempt 1 missing: {attempts:?}");
        assert!(attempts.contains(&2), "attempt 2 missing: {attempts:?}");
    }

    #[tokio::test]
    async fn attempt_is_allocated_before_spawn_failure() {
        let (ctx, _rx, calls) = recording_output_context();
        let runner = SubprocessRunner::new("test-runner");
        let task = mk_subprocess_spec("failed-spawn", "/definitely/not/a/command");
        let task_ref = runner.build_task(&task, &ctx).unwrap();

        assert!(task_ref.spawn(TaskContext::detached()).await.is_err());
        assert!(task_ref.spawn(TaskContext::detached()).await.is_err());

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[
                ("task-failed-spawn".into(), 1, 1),
                ("task-failed-spawn".into(), 1, 2),
            ]
        );
    }

    #[tokio::test]
    async fn run_subprocess_does_not_hang_on_daemonized_grandchild_holding_pipe() {
        use std::time::{Duration, Instant};

        let runner = SubprocessRunner::new("hang-runner");
        let spec = mk_subprocess_spec_with_args("hang-slot", "sh", &["-c", "sleep 30 & exit 0"]);
        let task_ref = runner.build_task(&spec, &BuildContext::default()).unwrap();

        let started = Instant::now();
        let ctx = TaskContext::detached();
        let res = tokio::time::timeout(Duration::from_secs(20), task_ref.spawn(ctx)).await;

        assert!(
            res.is_ok(),
            "run_subprocess hung past the bounded log-drain grace (daemonized grandchild)"
        );
        res.unwrap().expect("leader exited 0; task should succeed");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "run_subprocess took too long: {:?}",
            started.elapsed()
        );
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
            runtime: solti_model::Runtime::Bash,
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
            runtime: solti_model::Runtime::Bash,
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
