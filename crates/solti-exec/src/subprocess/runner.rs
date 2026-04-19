//! # Runner: subprocess execution engine.
//!
//! [`SubprocessRunner`] implements the [`Runner`](solti_runner::Runner) trait to execute
//! [`TaskKind::Subprocess`](solti_model::TaskKind::Subprocess) tasks as OS processes.
//!
//! ## How it works
//! ```text
//! SubprocessRunner::build_task(spec, ctx)
//!     │
//!     ├──► build_task_config(spec, ctx)
//!     │     ├──► match TaskKind::Subprocess(SubprocessSpec { .. })
//!     │     ├──► resolve_mode(mode) → Resolved { command, args, script_tempfile? }
//!     │     │     ├──► Command { command, args } → clone, tempfile = None
//!     │     │     └──► Script { runtime, body }  → decode base64
//!     │     │                                    → write to NamedTempFile (0600)
//!     │     │                                    → args = [path, extra...]
//!     │     │                                    → tempfile = Some(tmp)
//!     │     ├──► merge_env(task_env, runner_env)→ BTreeMap
//!     │     └──► SubprocessTaskConfig { run_id, seq, command, args, env, cwd, fail_on_non_zero }
//!     │
//!     ├──► prepare backend (cgroup dirs, if configured)
//!     │
//!     ├──► build Arc<TaskExecContext>
//!     │     └──► { task_cfg, runner_cfg, cgroup_name, metrics, log_cfg, script_tempfile? }
//!     │          tempfile kept alive until context drops → Drop = unlink
//!     │
//!     └──► return TaskFn closure → run_subprocess(ctx, cancel)
//! ```
//!
//! ## Subprocess execution lifecycle
//! ```text
//! run_subprocess(ctx, cancel)
//!     │
//!     ├──► metrics.record_task_started()
//!     ├──► prepare_backend() → create cgroup dirs
//!     ├──► build_command() → Command with:
//!     │      - args, env, cwd, piped stdout/stderr
//!     │      - process_group(0)   (Unix: new pgid for kill-whole-subtree)
//!     │      - kill_on_drop(true) (tokio SIGKILLs PID if future is dropped)
//!     ├──► apply_backend() → install pre_exec hooks (rlimits, cgroup join, security)
//!     ├──► cmd.spawn()
//!     │
//!     ├──► tokio::spawn(log_stream(stdout, Stdout))
//!     ├──► tokio::spawn(log_stream(stderr, Stderr))
//!     │
//!     ├──► select! { biased; ... }
//!     │     ├──► child.wait() → evaluate_exit(status)
//!     │     └──► cancel.cancelled() → kill_process_group() (killpg -SIGKILL pgid)
//!     │                             → child.wait() to reap zombie
//!     │
//!     ├──► metrics.record_task_completed(outcome, duration)
//!     ├──► join!(stdout_task, stderr_task)
//!     ├──► cleanup_cgroup() (if configured)
//!     └──► return result
//! ```

use std::{
    io::Write as _,
    process::Stdio,
    sync::Arc,
    time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

use taskvisor::{TaskError, TaskFn, TaskRef};
use tempfile::NamedTempFile;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use solti_model::{SubprocessSpec, TaskKind, TaskSpec, merge_env};
use solti_runner::{BuildContext, Runner, RunnerError, RunnerType};

use crate::metrics::classify_task_error;
use crate::subprocess::{
    backend::SubprocessBackendConfig,
    logger::{LogConfig, StreamKind, log_stream},
    task::SubprocessTaskConfig,
};

/// Runner that executes `TaskKind::Subprocess` as OS subprocesses.
///
/// ## Also
///
/// - [`SubprocessBackendConfig`] rlimits, cgroups, security applied to spawned processes.
/// - [`SubprocessTaskConfig`](super::SubprocessTaskConfig) resolved per-task config.
/// - [`register_subprocess_runner`](super::register_subprocess_runner) registration helper.
/// - [`solti_runner::Runner`] trait this type implements.
pub struct SubprocessRunner {
    /// Runner name.
    name: &'static str,
    /// Backend configuration applied to all tasks spawned by this runner.
    config: Option<Arc<SubprocessBackendConfig>>,
}

impl SubprocessRunner {
    /// Create a new subprocess runner without backend configuration.
    pub fn new(name: &'static str) -> Self {
        Self { name, config: None }
    }

    /// Create a subprocess runner with explicit backend configuration.
    ///
    /// # Errors
    ///
    /// Returns `ExecError` if the configuration is invalid.
    pub fn with_config(
        name: &'static str,
        config: SubprocessBackendConfig,
    ) -> Result<Self, crate::ExecError> {
        config.validate()?;
        Ok(Self {
            name,
            config: Some(Arc::new(config)),
        })
    }

    /// Build task configuration from `TaskSpec`.
    ///
    /// Returns the fully resolved config.
    fn build_task_config(
        &self,
        spec: &TaskSpec,
        ctx: &BuildContext,
    ) -> Result<(SubprocessTaskConfig, Option<NamedTempFile>), RunnerError> {
        let slot = spec.slot();
        let (cfg, script_tempfile) = match spec.kind() {
            TaskKind::Subprocess(SubprocessSpec {
                mode,
                env,
                cwd,
                fail_on_non_zero,
            }) => {
                let Resolved {
                    command,
                    args,
                    script_tempfile,
                } = Self::resolve_mode(mode)?;
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
                (cfg, script_tempfile)
            }
            other => {
                return Err(RunnerError::UnsupportedKind {
                    runner: self.name,
                    kind: other.kind().to_string(),
                });
            }
        };
        cfg.validate()
            .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;
        Ok((cfg, script_tempfile))
    }

    /// Resolve [`SubprocessMode`](solti_model::SubprocessMode) into a command + args pair ready for `execve`.
    ///
    /// ## Script transport
    ///
    /// For `Script` mode the body is written to a `NamedTempFile` (mode 0600) and the interpreter is invoked with the file path: *not* with `-c "<inline>"`.
    ///
    /// ## Limits
    ///
    /// - The inline form is limited to `MAX_ARG_STRLEN` (128 KiB on Linux);
    /// - The tempfile form supports scripts up to [`MAX_SCRIPT_BODY_BYTES`](solti_model::MAX_SCRIPT_BODY_BYTES) (2 MiB).
    fn resolve_mode(mode: &solti_model::SubprocessMode) -> Result<Resolved, RunnerError> {
        match mode {
            solti_model::SubprocessMode::Command { command, args } => Ok(Resolved {
                command: command.clone(),
                args: args.clone(),
                script_tempfile: None,
            }),
            solti_model::SubprocessMode::Script { runtime, args, .. } => {
                let script = mode
                    .decode_body()
                    .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;

                let mut tmp = NamedTempFile::with_prefix("solti-script-").map_err(|e| {
                    RunnerError::InvalidSpec(format!("failed to create script tempfile: {e}"))
                })?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o600);
                    if let Err(e) = tmp.as_file().set_permissions(perms) {
                        return Err(RunnerError::InvalidSpec(format!(
                            "failed to chmod 0600 script tempfile: {e}"
                        )));
                    }
                }

                tmp.write_all(script.as_bytes()).map_err(|e| {
                    RunnerError::InvalidSpec(format!("failed to write script body: {e}"))
                })?;
                tmp.as_file()
                    .sync_all()
                    .or_else(|_| tmp.as_file().flush())
                    .map_err(|e| {
                        RunnerError::InvalidSpec(format!("failed to flush script tempfile: {e}"))
                    })?;

                let (cmd, _flag_deprecated_for_tempfile_transport) = runtime.resolve();
                let path = tmp.path().to_string_lossy().into_owned();

                let mut full_args = Vec::with_capacity(1 + args.len());
                full_args.push(path);
                full_args.extend(args.iter().cloned());

                Ok(Resolved {
                    command: cmd.to_string(),
                    args: full_args,
                    script_tempfile: Some(tmp),
                })
            }
        }
    }
}

/// Output of [`SubprocessRunner::resolve_mode`].
#[derive(Debug)]
struct Resolved {
    command: String,
    args: Vec<String>,

    /// Tempfile holding the script body for `Script` mode; `None` for `Command`.
    script_tempfile: Option<NamedTempFile>,
}

impl Runner for SubprocessRunner {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind(), TaskKind::Subprocess(_))
    }

    fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let (task_cfg, script_tempfile) = self.build_task_config(spec, ctx)?;

        trace!(
            slot = %spec.slot(),
            task = %task_cfg.run_id,
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
                    spec.slot().as_str(),
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

            _script_tempfile: script_tempfile.map(Arc::new),
        });

        let run_id = exec_ctx.task_cfg.run_id.to_string();
        let task: TaskRef = TaskFn::arc(run_id, move |cancel: CancellationToken| {
            let ctx = Arc::clone(&exec_ctx);
            async move { run_subprocess(ctx, cancel).await }
        });
        Ok(task)
    }
}

/// Shared context for subprocess task execution.
struct TaskExecContext {
    task_cfg: SubprocessTaskConfig,
    runner_cfg: Option<Arc<SubprocessBackendConfig>>,
    cgroup_name: Option<String>,
    metrics: solti_runner::MetricsHandle,
    log_cfg: LogConfig,

    _script_tempfile: Option<Arc<NamedTempFile>>,
}

/// Build the OS command from task configuration.
fn build_command(ctx: &TaskExecContext) -> Command {
    let mut cmd = Command::new(&ctx.task_cfg.command);
    cmd.args(&ctx.task_cfg.args);
    if let Some(cwd) = &ctx.task_cfg.cwd {
        cmd.current_dir(cwd);
    }
    cmd.envs(&ctx.task_cfg.env);
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

/// Kill of the entire process group led by `child`.
///
/// On Unix: `killpg(pid, SIGKILL)` via `libc::kill(-pid, SIGKILL)`
///
/// On other platforms: falls back to `child.kill()` (single PID only).
async fn kill_process_group(child: &mut tokio::process::Child, run_id: &str) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
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
        } else {
            // No pid means the child was already reaped - nothing to kill.
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
}

/// Prepare backend resources (cgroup directories) before spawn.
fn prepare_backend(ctx: &TaskExecContext) -> Result<(), TaskError> {
    if let Some(backend_cfg) = &ctx.runner_cfg {
        let cgroup_name_ref = ctx.cgroup_name.as_deref().unwrap_or(&ctx.task_cfg.run_id);

        if let Err(e) = backend_cfg.prepare_cgroups(cgroup_name_ref) {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, "cgroup_prepare_failed");
            return Err(TaskError::Fatal {
                reason: format!("failed to prepare cgroup: {e}"),
                exit_code: None,
            });
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
                .record_runner_error(RunnerType::Subprocess, "backend_config_failed");
            return Err(TaskError::Fatal {
                reason: format!("failed to apply runner config: {e}"),
                exit_code: None,
            });
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
        Err(TaskError::Fail { reason, exit_code })
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

/// Execute a subprocess task with cancellation support, metrics, and cleanup.
async fn run_subprocess(
    ctx: Arc<TaskExecContext>,
    cancel: CancellationToken,
) -> Result<(), TaskError> {
    ctx.metrics.record_task_started(RunnerType::Subprocess);
    let start = Instant::now();

    trace!(
        task = %ctx.task_cfg.run_id,
        command = %ctx.task_cfg.command,
        args = ?ctx.task_cfg.args,
        cwd = ?ctx.task_cfg.cwd,
        "spawning subprocess",
    );

    prepare_backend(&ctx)?;

    let _cgroup_guard = CgroupGuard(ctx.cgroup_name.as_deref());
    let mut cmd = build_command(&ctx);
    apply_backend(&mut cmd, &ctx)?;

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            ctx.metrics
                .record_runner_error(RunnerType::Subprocess, "spawn_failed");
            return Err(TaskError::Fatal {
                reason: format!("spawn failed: {e}"),
                exit_code: None,
            });
        }
    };

    let log_cfg = ctx.log_cfg;

    let stdout = child.stdout.take().ok_or_else(|| TaskError::Fatal {
        reason: "failed to capture stdout".into(),
        exit_code: None,
    })?;
    let run_id_stdout = Arc::clone(&ctx.task_cfg.run_id);
    let stdout_task = tokio::spawn(async move {
        log_stream(stdout, &run_id_stdout, StreamKind::Stdout, &log_cfg).await;
    });

    let stderr = child.stderr.take().ok_or_else(|| TaskError::Fatal {
        reason: "failed to capture stderr".into(),
        exit_code: None,
    })?;
    let run_id_stderr = Arc::clone(&ctx.task_cfg.run_id);
    let stderr_task = tokio::spawn(async move {
        log_stream(stderr, &run_id_stderr, StreamKind::Stderr, &log_cfg).await;
    });

    let result = tokio::select! {
        biased;
        res = child.wait() => {
            let status = res.map_err(|e| TaskError::Fatal {
                reason: format!("wait failed: {e}"),
                exit_code: None,
            })?;
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

    let duration_ms = start.elapsed().as_millis() as u64;
    let outcome = match &result {
        Ok(()) => solti_runner::TaskOutcome::Success,
        Err(e) => classify_task_error(e),
    };
    ctx.metrics
        .record_task_completed(RunnerType::Subprocess, outcome, duration_ms);

    let _ = tokio::join!(stdout_task, stderr_task);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_backoff() -> solti_model::BackoffPolicy {
        solti_model::BackoffPolicy {
            jitter: solti_model::JitterPolicy::Equal,
            first_ms: 100,
            max_ms: 1000,
            factor: 2.0,
        }
    }

    fn mk_subprocess_spec(slot: &str, command: &str) -> TaskSpec {
        TaskSpec::builder(
            slot,
            TaskKind::Subprocess(SubprocessSpec {
                mode: solti_model::SubprocessMode::Command {
                    command: command.into(),
                    args: vec![],
                },
                env: Default::default(),
                cwd: None,
                fail_on_non_zero: Default::default(),
            }),
            5_000u64,
        )
        .restart(solti_model::RestartPolicy::Never)
        .backoff(mk_backoff())
        .admission(solti_model::AdmissionPolicy::DropIfRunning)
        .build()
        .unwrap()
    }

    fn mk_embedded_spec(slot: &str) -> TaskSpec {
        TaskSpec::builder(slot, TaskKind::Embedded, 5_000u64)
            .restart(solti_model::RestartPolicy::Never)
            .backoff(mk_backoff())
            .admission(solti_model::AdmissionPolicy::DropIfRunning)
            .build()
            .unwrap()
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
            _script_tempfile: None,
        }
    }

    #[test]
    fn build_command_sets_args_and_pipes() {
        let ctx = make_exec_ctx();
        let cmd = build_command(&ctx);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "echo");
        let args: Vec<_> = std_cmd.get_args().collect();
        assert_eq!(args, vec!["hello"]);
    }

    #[test]
    fn build_command_sets_env() {
        let mut ctx = make_exec_ctx();
        ctx.task_cfg.env.insert("FOO".into(), "bar".into());
        let cmd = build_command(&ctx);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "FOO" && *v == Some(std::ffi::OsStr::new("bar")))
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
            TaskError::Fail { reason, exit_code } => {
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
        let spec = mk_subprocess_spec("test-slot", "echo");
        let result = runner.build_task(&spec, &BuildContext::default());
        assert!(result.is_ok());
    }

    #[test]
    fn build_task_rejects_non_subprocess_kind() {
        let runner = SubprocessRunner::new("test-runner");
        let spec = mk_embedded_spec("test-slot");
        match runner.build_task(&spec, &BuildContext::default()) {
            Err(RunnerError::UnsupportedKind { runner, kind }) => {
                assert_eq!(runner, "test-runner");
                assert_eq!(kind, "embedded");
            }
            Err(other) => panic!("expected UnsupportedKind, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn supports_returns_true_for_subprocess() {
        let runner = SubprocessRunner::new("test");
        assert!(runner.supports(&mk_subprocess_spec("s", "echo")));
    }

    #[test]
    fn supports_returns_false_for_embedded() {
        let runner = SubprocessRunner::new("test");
        assert!(!runner.supports(&mk_embedded_spec("s")));
    }

    #[test]
    fn build_task_returns_task_ref_for_script_mode() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let runner = SubprocessRunner::new("test-runner");
        let spec = TaskSpec::builder(
            "test-slot",
            TaskKind::Subprocess(solti_model::SubprocessSpec {
                mode: solti_model::SubprocessMode::Script {
                    runtime: solti_model::Runtime::Bash,
                    body: BASE64.encode(b"echo hello"),
                    args: vec![],
                },
                env: Default::default(),
                cwd: None,
                fail_on_non_zero: Default::default(),
            }),
            5_000u64,
        )
        .restart(solti_model::RestartPolicy::Never)
        .backoff(mk_backoff())
        .admission(solti_model::AdmissionPolicy::DropIfRunning)
        .build()
        .unwrap();
        let result = runner.build_task(&spec, &BuildContext::default());
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_mode_command() {
        let mode = solti_model::SubprocessMode::Command {
            command: "ls".into(),
            args: vec!["-la".into()],
        };
        let r = SubprocessRunner::resolve_mode(&mode).unwrap();
        assert_eq!(r.command, "ls");
        assert_eq!(r.args, vec!["-la"]);
        assert!(
            r.script_tempfile.is_none(),
            "Command mode needs no tempfile"
        );
    }

    #[test]
    fn resolve_mode_script_bash_uses_tempfile() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let mode = solti_model::SubprocessMode::Script {
            runtime: solti_model::Runtime::Bash,
            body: BASE64.encode(b"echo hello"),
            args: vec!["extra".into()],
        };
        let r = SubprocessRunner::resolve_mode(&mode).unwrap();
        assert_eq!(r.command, "bash");
        assert_eq!(r.args.len(), 2, "args: {:?}", r.args);
        assert_eq!(r.args[1], "extra");

        let tmp = r
            .script_tempfile
            .expect("Script mode must produce a tempfile");
        assert_eq!(tmp.path().to_string_lossy(), r.args[0]);
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
        let r = SubprocessRunner::resolve_mode(&mode).unwrap();
        assert_eq!(r.command, "ruby");
        assert_eq!(r.args.len(), 1, "only the tempfile path, no flag");
        assert!(!r.args[0].contains("-e"), "flag must not leak into args");
        assert!(r.script_tempfile.is_some());
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
                if let Ok(s) = std::fs::read_to_string(&marker) {
                    if let Some(line) = s.trim().lines().next() {
                        if let Ok(pid) = line.parse::<i32>() {
                            break pid;
                        }
                    }
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

    #[test]
    fn resolve_mode_invalid_base64() {
        let mode = solti_model::SubprocessMode::Script {
            runtime: solti_model::Runtime::Bash,
            body: "not-valid!!!".into(),
            args: vec![],
        };
        let err = SubprocessRunner::resolve_mode(&mode).unwrap_err();
        assert!(matches!(err, RunnerError::InvalidSpec(_)));
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
        let r = SubprocessRunner::resolve_mode(&mode)
            .expect("200 KiB script must resolve via tempfile");
        assert_eq!(r.command, "bash");
        assert_eq!(r.args.len(), 1);
        let tmp = r
            .script_tempfile
            .expect("large Script must allocate a tempfile");
        let written = std::fs::read(tmp.path()).unwrap();
        assert_eq!(written.len(), payload.len());
    }
}
