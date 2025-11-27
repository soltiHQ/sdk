use std::process::Stdio;

use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use tno_core::{BuildContext, Runner, RunnerError};
use tno_model::{CreateSpec, TaskKind};

use crate::subprocess::config::SubprocessConfig;

/// Runner that executes `TaskKind::Subprocess` as OS subprocesses.
pub struct SubprocessRunner {
    name: &'static str,
}

impl SubprocessRunner {
    /// Create a new subprocess with name.
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }

    /// Build normalized subprocess configuration from `CreateSpec` + `BuildContext`.
    fn build_config(
        &self,
        spec: &CreateSpec,
        ctx: &BuildContext,
    ) -> Result<SubprocessConfig, RunnerError> {
        let cfg = match &spec.kind {
            TaskKind::Subprocess {
                command,
                args,
                env,
                cwd,
                fail_on_non_zero,
            } => SubprocessConfig {
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
        cfg.trace_state(&spec.slot);
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
        let cfg = self.build_config(spec, ctx)?;
        let slot = spec.slot.clone();

        trace!(
            slot = %slot,
            "building subprocess task",
        );

        let task: TaskRef = TaskFn::arc(slot, move |cancel: CancellationToken| {
            let cfg = cfg.clone();

            async move {
                trace!(
                    command = %cfg.command,
                    args = ?cfg.args,
                    cwd = ?cfg.cwd,
                    "spawning subprocess",
                );

                let mut cmd = Command::new(&cfg.command);
                cmd.args(&cfg.args);

                if let Some(cwd) = &cfg.cwd {
                    cmd.current_dir(cwd);
                }
                for kv in cfg.env.iter() {
                    cmd.env(kv.key(), kv.value());
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::inherit());

                let mut child = cmd.spawn().map_err(|e| TaskError::Fatal {
                    reason: format!("spawn failed: {e}"),
                })?;
                let status_fut = child.wait();

                tokio::select! {
                    res = status_fut => {
                        let status = res.map_err(|e| TaskError::Fatal {
                            reason: format!("wait failed: {e}"),
                        })?;

                        if !status.success() && cfg.fail_on_non_zero.is_enabled() {
                            return if let Some(code) = status.code() {
                                Err(TaskError::Fail {
                                    reason: format!("process exited with non-zero code: {code}"),
                                })
                            } else {
                                Err(TaskError::Fail {
                                    reason: "process terminated by signal".into(),
                                })
                            }
                        }
                        debug!("subprocess exited successfully");
                        Ok(())
                    }
                    _ = cancel.cancelled() => {
                        debug!("cancellation requested; killing subprocess");

                        if let Err(e) = child.kill().await {
                            debug!("failed to kill subprocess: {e}");
                        }
                        Err(TaskError::Canceled)
                    }
                }
            }
        });
        Ok(task)
    }
}
