use std::process::Stdio;

use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use tno_core::{BuildContext, Runner, RunnerError};
use tno_model::{CreateSpec, TaskKind};

use crate::subprocess::{backend::SubprocessBackendConfig, task::SubprocessTaskConfig};

/// Runner that executes `TaskKind::Subprocess` as OS subprocesses.
pub struct SubprocessRunner {
    /// Runner name.
    name: &'static str,
    /// Configuration applied to all tasks spawned by this runner.
    config: Option<SubprocessBackendConfig>,
}

impl SubprocessRunner {
    /// Create a new subprocess with name.
    pub fn new(name: &'static str) -> Self {
        Self { name, config: None }
    }

    /// Create a subprocess runner with explicit backend configuration.
    ///
    /// Backend settings (rlimits, cgroups, security) will be applied to
    /// all tasks spawned by this runner instance.
    pub fn with_config(name: &'static str, config: SubprocessBackendConfig) -> Self {
        Self {
            name,
            config: Some(config),
        }
    }

    /// Build configuration from `CreateSpec`.
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
            slot = %spec.slot.clone(),
            task = %task_cfg.run_id,
            "building subprocess task",
        );

        let task: TaskRef = TaskFn::arc(
            task_cfg.run_id.clone(),
            move |cancel: CancellationToken| {
                let task_cfg = task_cfg.clone();
                let runner_cfg = runner_cfg.clone();

                async move {
                    let mut cmd = Command::new(&task_cfg.command);
                    cmd.args(&task_cfg.args);

                    trace!(
                        task = %task_cfg.run_id,
                        command = %task_cfg.command,
                        args = ?task_cfg.args,
                        cwd = ?task_cfg.cwd,
                        "spawning subprocess",
                    );

                    if let Some(cwd) = &task_cfg.cwd {
                        cmd.current_dir(cwd);
                    }
                    for kv in task_cfg.env.iter() {
                        cmd.env(kv.key(), kv.value());
                    }
                    cmd.stdout(Stdio::piped());
                    cmd.stderr(Stdio::inherit());

                    if let Some(runner_cfg) = &runner_cfg {
                        runner_cfg
                            .apply_to_command(&mut cmd, &task_cfg.run_id)
                            .map_err(|e| TaskError::Fatal {
                                reason: format!("failed apply runner configs: {e}"),
                            })?;
                    }
                    let mut child = cmd.spawn().map_err(|e| TaskError::Fatal {
                        reason: format!("spawn failed: {e}"),
                    })?;

                    let status_fut = child.wait();
                    tokio::select! {
                        res = status_fut => {
                            let status = res.map_err(|e| TaskError::Fatal {
                                reason: format!("wait failed: {e}"),
                            })?;
                            if !status.success() && task_cfg.fail_on_non_zero.is_enabled() {
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
            },
        );
        Ok(task)
    }
}
