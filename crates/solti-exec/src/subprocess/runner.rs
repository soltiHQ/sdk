use std::{
    borrow::Cow,
    process::Stdio,
    sync::Arc,
    time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH},
};

use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use solti_core::{BuildContext, Runner, RunnerError};
use solti_model::{TaskKind, TaskSpec, merge_env};

use crate::metrics::{RUNNER_TYPE_SUBPROCESS, task_error_to_outcome};
use crate::subprocess::{
    backend::SubprocessBackendConfig, logger::LogConfig, task::SubprocessTaskConfig,
};

/// Runner that executes `TaskKind::Subprocess` as OS subprocesses.
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
    fn build_task_config(
        &self,
        spec: &TaskSpec,
        ctx: &BuildContext,
    ) -> Result<SubprocessTaskConfig, RunnerError> {
        let slot = &spec.slot;
        let cfg = match &spec.kind {
            TaskKind::Subprocess {
                command,
                args,
                env,
                cwd,
                fail_on_non_zero,
            } => SubprocessTaskConfig {
                run_id: Arc::from(self.build_run_id(slot.as_str())),
                command: command.clone(),
                args: args.clone(),
                env: merge_env(env, ctx.env()),
                cwd: cwd.clone(),
                fail_on_non_zero: *fail_on_non_zero,
            },
            other => {
                return Err(RunnerError::UnsupportedKind {
                    runner: self.name,
                    kind: other.kind().to_string(),
                });
            }
        };
        cfg.validate()
            .map_err(|e| RunnerError::InvalidSpec(e.to_string()))?;
        Ok(cfg)
    }
}

impl Runner for SubprocessRunner {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind, TaskKind::Subprocess { .. })
    }

    fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let task_cfg = self.build_task_config(spec, ctx)?;

        trace!(
            slot = %spec.slot,
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
                    spec.slot.as_str(),
                    extract_seq_from_run_id(&task_cfg.run_id),
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
    metrics: solti_core::MetricsHandle,
    log_cfg: LogConfig,
}

/// Build the OS command from task configuration.
fn build_command(ctx: &TaskExecContext) -> Command {
    let mut cmd = Command::new(&ctx.task_cfg.command);
    cmd.args(&ctx.task_cfg.args);
    if let Some(cwd) = &ctx.task_cfg.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &ctx.task_cfg.env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Prepare backend resources (cgroup directories) before spawn.
fn prepare_backend(ctx: &TaskExecContext) -> Result<(), TaskError> {
    if let Some(backend_cfg) = &ctx.runner_cfg {
        let cgroup_name_ref = ctx.cgroup_name.as_deref().unwrap_or(&ctx.task_cfg.run_id);
        if let Err(e) = backend_cfg.prepare_cgroups(cgroup_name_ref) {
            ctx.metrics
                .record_runner_error(RUNNER_TYPE_SUBPROCESS, "cgroup_prepare_failed");
            return Err(TaskError::Fatal {
                reason: format!("failed to prepare cgroup: {e}"),
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
                .record_runner_error(RUNNER_TYPE_SUBPROCESS, "backend_config_failed");
            return Err(TaskError::Fatal {
                reason: format!("failed to apply runner config: {e}"),
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
        let reason = match status.code() {
            Some(code) => format!("process exited with non-zero code: {code}"),
            None => "process terminated by signal".into(),
        };
        Err(TaskError::Fail { reason })
    } else {
        debug!(task = %task_cfg.run_id, "subprocess exited successfully");
        Ok(())
    }
}

/// Execute a subprocess task with cancellation support, metrics, and cleanup.
async fn run_subprocess(
    ctx: Arc<TaskExecContext>,
    cancel: CancellationToken,
) -> Result<(), TaskError> {
    ctx.metrics.record_task_started(RUNNER_TYPE_SUBPROCESS);
    let start = Instant::now();

    trace!(
        task = %ctx.task_cfg.run_id,
        command = %ctx.task_cfg.command,
        args = ?ctx.task_cfg.args,
        cwd = ?ctx.task_cfg.cwd,
        "spawning subprocess",
    );

    prepare_backend(&ctx)?;

    let mut cmd = build_command(&ctx);
    apply_backend(&mut cmd, &ctx)?;

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            ctx.metrics
                .record_runner_error(RUNNER_TYPE_SUBPROCESS, "spawn_failed");
            return Err(TaskError::Fatal {
                reason: format!("spawn failed: {e}"),
            });
        }
    };

    let log_cfg = ctx.log_cfg;

    let stdout = child.stdout.take().ok_or_else(|| TaskError::Fatal {
        reason: "failed to capture stdout".into(),
    })?;
    let run_id_stdout = Arc::clone(&ctx.task_cfg.run_id);
    let stdout_task = tokio::spawn(async move {
        log_stream(stdout, &run_id_stdout, StreamKind::Stdout, &log_cfg).await;
    });

    let stderr = child.stderr.take().ok_or_else(|| TaskError::Fatal {
        reason: "failed to capture stderr".into(),
    })?;
    let run_id_stderr = Arc::clone(&ctx.task_cfg.run_id);
    let stderr_task = tokio::spawn(async move {
        log_stream(stderr, &run_id_stderr, StreamKind::Stderr, &log_cfg).await;
    });

    let status_fut = child.wait();
    let result = tokio::select! {
        res = status_fut => {
            let status = res.map_err(|e| TaskError::Fatal {
                reason: format!("wait failed: {e}"),
            })?;
            evaluate_exit(status, &ctx.task_cfg)
        }
        _ = cancel.cancelled() => {
            debug!(task = %ctx.task_cfg.run_id, "cancellation requested; killing subprocess");
            if let Err(e) = child.kill().await {
                debug!(task = %ctx.task_cfg.run_id, "failed to kill subprocess: {e}");
            }
            Err(TaskError::Canceled)
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let outcome = match &result {
        Ok(()) => solti_core::TaskOutcome::Success,
        Err(e) => task_error_to_outcome(e),
    };
    ctx.metrics
        .record_task_completed(RUNNER_TYPE_SUBPROCESS, outcome, duration_ms);

    let _ = tokio::join!(stdout_task, stderr_task);
    if let Some(cgroup_name) = &ctx.cgroup_name {
        let _ = crate::utils::cleanup_cgroup(cgroup_name);
    }
    result
}

/// Truncate line by Unicode scalar count, safe for UTF-8.
///
/// Returns `Cow::Borrowed` when no truncation is needed (zero-alloc for the common case).
fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        None => Cow::Borrowed(line),
        Some((i, _)) => {
            let skipped = line[i..].chars().count();
            Cow::Owned(format!("{}... (truncated {skipped} chars)", &line[..i]))
        }
    }
}

/// Subprocess output stream kind.
#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn use_elevated_level(self, config: &LogConfig) -> bool {
        match self {
            Self::Stdout => config.stdout_info,
            Self::Stderr => config.stderr_warn,
        }
    }
}

/// Log subprocess output stream with truncation.
async fn log_stream<R>(reader: R, run_id: &str, stream: StreamKind, config: &LogConfig)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let stream_name = stream.as_str();
    let mut lines = BufReader::new(reader).lines();
    let mut line_count = 0u64;

    while let Some(result) = lines.next_line().await.transpose() {
        let raw_line = match result {
            Ok(line) => line,
            Err(e) => {
                warn!(
                    task = %run_id,
                    stream = %stream_name,
                    error = %e,
                    line_num = line_count,
                    "error while reading subprocess stream"
                );
                break;
            }
        };

        // max_line_length is guaranteed > 0 by SubprocessBackendConfig::validate().
        let line = truncate_line(&raw_line, config.max_line_length);

        line_count += 1;

        if stream.use_elevated_level(config) {
            match stream {
                StreamKind::Stdout => info!(
                    task = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    "{}",
                    line
                ),
                StreamKind::Stderr => warn!(
                    task = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    "{}",
                    line
                ),
            }
        } else {
            debug!(
                task = %run_id,
                stream = %stream_name,
                line_num = line_count,
                "{}",
                line
            );
        }
    }

    debug!(
        task = %run_id,
        stream = %stream_name,
        total_lines = line_count,
        "stream closed"
    );
}

/// Extract sequence number from run_id.
fn extract_seq_from_run_id(run_id: &str) -> u64 {
    run_id
        .rsplit('-')
        .next()
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_line_short_line_borrowed() {
        let result = truncate_line("hello", 10);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "hello");
    }

    #[test]
    fn truncate_line_exact_length_borrowed() {
        let result = truncate_line("hello", 5);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "hello");
    }

    #[test]
    fn truncate_line_truncates_long_line() {
        let result = truncate_line("hello world", 5);
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(&*result, "hello... (truncated 6 chars)");
    }

    #[test]
    fn truncate_line_empty_string_borrowed() {
        let result = truncate_line("", 10);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "");
    }

    #[test]
    fn truncate_line_unicode() {
        let result = truncate_line("привет", 2);
        assert_eq!(&*result, "пр... (truncated 4 chars)");
    }

    #[test]
    fn truncate_line_single_char_limit() {
        let result = truncate_line("abc", 1);
        assert_eq!(&*result, "a... (truncated 2 chars)");
    }

    #[test]
    fn extract_seq_from_run_id_valid() {
        assert_eq!(extract_seq_from_run_id("runner-slot-ff"), 255);
    }

    #[test]
    fn extract_seq_from_run_id_no_hex() {
        assert_eq!(extract_seq_from_run_id("no-hex-zzz"), 0);
    }

    #[test]
    fn extract_seq_from_run_id_empty() {
        assert_eq!(extract_seq_from_run_id(""), 0);
    }

    fn mk_backoff() -> solti_model::BackoffPolicy {
        solti_model::BackoffPolicy {
            jitter: solti_model::JitterPolicy::Equal,
            first_ms: 100,
            max_ms: 1000,
            factor: 2.0,
        }
    }

    fn mk_subprocess_spec(slot: &str, command: &str) -> TaskSpec {
        TaskSpec {
            slot: slot.into(),
            kind: TaskKind::Subprocess {
                command: command.into(),
                args: vec![],
                env: Default::default(),
                cwd: None,
                fail_on_non_zero: Default::default(),
            },
            timeout: 5_000_u64.into(),
            restart: solti_model::RestartPolicy::Never,
            backoff: mk_backoff(),
            admission: solti_model::AdmissionPolicy::DropIfRunning,
            runner_selector: None,
            labels: Default::default(),
        }
    }

    fn mk_embedded_spec(slot: &str) -> TaskSpec {
        TaskSpec {
            slot: slot.into(),
            kind: TaskKind::Embedded,
            timeout: 5_000_u64.into(),
            restart: solti_model::RestartPolicy::Never,
            backoff: mk_backoff(),
            admission: solti_model::AdmissionPolicy::DropIfRunning,
            runner_selector: None,
            labels: Default::default(),
        }
    }

    fn make_task_cfg() -> SubprocessTaskConfig {
        SubprocessTaskConfig {
            run_id: Arc::from("test-run-1"),
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
            metrics: solti_core::noop_metrics(),
            log_cfg: LogConfig::default(),
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
            TaskError::Fail { reason } => assert!(reason.contains("non-zero")),
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
}
