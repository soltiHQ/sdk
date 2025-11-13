use std::{path::PathBuf, sync::Arc};
use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tracing::{debug, trace};

use tno_core::runner::{BuildContext, Runner, RunnerError};
use tno_model::TaskKind;

use crate::util::kill_graceful;

/// ShellRunner: запускает строку в шеле (`sh -c` / `cmd /C`).
pub struct ShellRunner {
    name: &'static str,
    pub script: String,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub fail_on_non_zero: bool,
}

impl ShellRunner {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            name: "shell",
            script: script.into(),
            env: Vec::new(),
            cwd: None,
            fail_on_non_zero: true,
        }
    }

    pub fn with_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
}

impl Runner for ShellRunner {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, spec: &tno_model::CreateSpec) -> bool {
        matches!(spec.kind, TaskKind::Exec) // shell — это разновидность Exec
    }

    fn build_task(
        &self,
        _spec: &tno_model::CreateSpec,
        _ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        if self.script.trim().is_empty() {
            return Err(RunnerError::InvalidSpec("empty shell script".into()));
        }
        let script = self.script.clone();
        let env = self.env.clone();
        let cwd = self.cwd.clone();
        let fail_on_non_zero = self.fail_on_non_zero;
        let name = self.name;

        let task = TaskFn::new(move |cancel: tokio_util::sync::CancellationToken| {
            let script = script.clone();
            let env = env.clone();
            let cwd = cwd.clone();
            async move {
                cfg_if::cfg_if! {
                    if #[cfg(target_family = "windows")] {
                        let mut cmd = Command::new("cmd");
                        cmd.arg("/C").arg(&script);
                    } else {
                        let mut cmd = Command::new("sh");
                        cmd.arg("-c").arg(&script);
                        println!("aaaaa")
                    }
                }

                if let Some(cwd) = &cwd {
                    cmd.current_dir(cwd);
                }
                for (k, v) in &env {
                    cmd.env(k, v);
                }

                trace!(target: "tno.exec.shell", %script, "spawn");
                let mut child = cmd
                    .spawn()
                    .map_err(|e| TaskError::fatal(format!("spawn: {e}")))?;

                tokio::select! {
                    status = child.wait() => {
                        let status = status.map_err(|e| TaskError::fatal(format!("wait: {e}")))?;
                        if !status.success() && fail_on_non_zero {
                            if let Some(code) = status.code() {
                                debug!(target: "tno.exec.shell", code, "exit non-zero");
                                return Err(TaskError::non_fatal(format!("exit code: {code}")));
                            } else {
                                return Err(TaskError::non_fatal("terminated by signal"));
                            }
                        }
                        Ok(())
                    }
                    _ = cancel.cancelled() => {
                        debug!(target: "tno.exec.shell", "cancelled; killing child");
                        let _ = kill_graceful(&mut child).await;
                        Err(TaskError::non_fatal("cancelled"))
                    }
                }
            }
        });

        Ok(Arc::new(task))
    }
}
