use std::{
    process::Stdio,
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use tno_core::{BuildContext, Runner, RunnerError};
use tno_model::{CreateSpec, TaskKind};

use crate::subprocess::{
    backend::SubprocessBackendConfig, logger::LogConfig, task::SubprocessTaskConfig,
};

/// Runner that executes `TaskKind::Subprocess` as OS subprocesses.
pub struct SubprocessRunner {
    /// Runner name.
    name: &'static str,
    /// Backend configuration applied to all tasks spawned by this runner.
    config: Option<SubprocessBackendConfig>,
}

impl SubprocessRunner {
    /// Create a new subprocess runner without backend configuration.
    pub fn new(name: &'static str) -> Self {
        Self { name, config: None }
    }

    /// Create a subprocess runner with explicit backend configuration.
    pub fn with_config(name: &'static str, config: SubprocessBackendConfig) -> Self {
        Self {
            name,
            config: Some(config),
        }
    }

    /// Build task configuration from `CreateSpec`.
    fn build_task_config(
        &self,
        spec: &CreateSpec,
        ctx: &BuildContext,
    ) -> Result<SubprocessTaskConfig, RunnerError> {
        let cfg = match &spec.kind {
            TaskKind::Subprocess {
                command,
                args,
                env,
                cwd,
                fail_on_non_zero,
            } => SubprocessTaskConfig {
                run_id: self.build_run_id(&spec.slot),
                command: command.clone(),
                args: args.clone(),
                env: ctx.env().merged(env),
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

    fn supports(&self, spec: &CreateSpec) -> bool {
        matches!(spec.kind, TaskKind::Subprocess { .. })
    }

    fn build_task(&self, spec: &CreateSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let task_cfg = self.build_task_config(spec, ctx)?;
        let runner_cfg = self.config.clone();

        trace!(
            slot = %spec.slot,
            task = %task_cfg.run_id,
            "building subprocess task",
        );

        let cgroup_name = if let Some(backend_cfg) = &runner_cfg {
            if backend_cfg.has_cgroups() {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(StdDuration::from_secs(0))
                    .as_secs();

                Some(crate::utils::build_cgroup_name(
                    self.name,
                    &spec.slot,
                    extract_seq_from_run_id(&task_cfg.run_id),
                    timestamp,
                ))
            } else {
                None
            }
        } else {
            None
        };

        let task: TaskRef = TaskFn::arc(
            task_cfg.run_id.clone(),
            move |cancel: CancellationToken| {
                let task_cfg = task_cfg.clone();
                let runner_cfg = runner_cfg.clone();
                let cgroup_name = cgroup_name.clone();

                async move {
                    trace!(
                        task = %task_cfg.run_id,
                        command = %task_cfg.command,
                        args = ?task_cfg.args,
                        cwd = ?task_cfg.cwd,
                        "spawning subprocess",
                    );

                    let mut cmd = Command::new(&task_cfg.command);
                    cmd.args(&task_cfg.args);
                    if let Some(cwd) = &task_cfg.cwd {
                        cmd.current_dir(cwd);
                    }
                    for kv in task_cfg.env.iter() {
                        cmd.env(kv.key(), kv.value());
                    }
                    cmd.stdout(Stdio::piped());
                    cmd.stderr(Stdio::piped());

                    if let Some(backend_cfg) = &runner_cfg {
                        let cgroup_name_ref = cgroup_name.as_deref().unwrap_or(&task_cfg.run_id);
                        backend_cfg
                            .apply_to_command(&mut cmd, cgroup_name_ref)
                            .map_err(|e| TaskError::Fatal {
                                reason: format!("failed to apply runner config: {e}"),
                            })?;
                    }
                    let mut child = cmd.spawn().map_err(|e| TaskError::Fatal {
                        reason: format!("spawn failed: {e}"),
                    })?;

                    let log_cfg = runner_cfg
                        .as_ref()
                        .map(|c| *c.log_config())
                        .unwrap_or_default();

                    let stdout = child.stdout.take().ok_or_else(|| TaskError::Fatal {
                        reason: "failed to capture stdout".into(),
                    })?;
                    let run_id_stdout = task_cfg.run_id.clone();
                    let stdout_task = tokio::spawn(async move {
                        log_stream(stdout, &run_id_stdout, "stdout", &log_cfg).await;
                    });

                    let stderr = child.stderr.take().ok_or_else(|| TaskError::Fatal {
                        reason: "failed to capture stderr".into(),
                    })?;
                    let run_id_stderr = task_cfg.run_id.clone();
                    let stderr_task = tokio::spawn(async move {
                        log_stream(stderr, &run_id_stderr, "stderr", &log_cfg).await;
                    });

                    let status_fut = child.wait();
                    let result = tokio::select! {
                        res = status_fut => {
                            let status = res.map_err(|e| TaskError::Fatal {
                                reason: format!("wait failed: {e}"),
                            })?;
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
                        _ = cancel.cancelled() => {
                            debug!(task = %task_cfg.run_id, "cancellation requested; killing subprocess");
                            if let Err(e) = child.kill().await {
                                debug!(task = %task_cfg.run_id, "failed to kill subprocess: {e}");
                            }
                            Err(TaskError::Canceled)
                        }
                    };
                    let _ = tokio::join!(stdout_task, stderr_task);
                    if let Some(cgroup_name) = cgroup_name {
                        let _ = crate::utils::cleanup_cgroup(&cgroup_name);
                    }
                    result
                }
            },
        );
        Ok(task)
    }
}

/// Truncate line by Unicode scalar count, safe for UTF-8.
///
/// If `max_chars` is 0, the caller should not invoke this function.
fn truncate_line(line: &str, max_chars: usize) -> String {
    let total = line.chars().count();
    if total <= max_chars {
        return line.to_owned();
    }

    let truncated: String = line.chars().take(max_chars).collect();
    let skipped = total - max_chars;

    format!("{truncated}... (truncated {skipped} chars)")
}

/// Log subprocess output stream with truncation.
async fn log_stream<R>(reader: R, run_id: &str, stream: &str, config: &LogConfig)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut line_count = 0u64;

    while let Some(result) = lines.next_line().await.transpose() {
        let raw_line = match result {
            Ok(line) => line,
            Err(e) => {
                warn!(
                    task = %run_id,
                    stream = %stream,
                    error = %e,
                    line_num = line_count,
                    "error while reading subprocess stream"
                );
                break;
            }
        };

        let line = if config.max_line_length > 0 {
            truncate_line(&raw_line, config.max_line_length)
        } else {
            raw_line
        };

        line_count += 1;

        match stream {
            "stdout" => {
                if config.stdout_info {
                    info!(
                        task = %run_id,
                        stream = "stdout",
                        line_num = line_count,
                        "{}",
                        line
                    );
                } else {
                    debug!(
                        task = %run_id,
                        stream = "stdout",
                        line_num = line_count,
                        "{}",
                        line
                    );
                }
            }
            "stderr" => {
                if config.stderr_warn {
                    warn!(
                        task = %run_id,
                        stream = "stderr",
                        line_num = line_count,
                        "{}",
                        line
                    );
                } else {
                    debug!(
                        task = %run_id,
                        stream = "stderr",
                        line_num = line_count,
                        "{}",
                        line
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    debug!(
        task = %run_id,
        stream = %stream,
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
