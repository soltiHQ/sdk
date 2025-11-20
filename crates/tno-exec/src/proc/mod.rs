use std::{path::PathBuf, sync::Arc};
use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use tno_core::{BuildContext, Runner, RunnerError};
use tno_model::TaskKind;

use crate::{
    error::ExecError,
    util::{cmd_program, kill_graceful},
};

/// Конфигурация процесса (вшитая в раннер инстанса).
#[derive(Clone, Debug)]
pub struct ProcConfig {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// Возвращать ошибку, если exit code != 0
    pub fail_on_non_zero: bool,
}

impl Default for ProcConfig {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            fail_on_non_zero: true,
        }
    }
}

/// Runner для TaskKind::Exec.
/// Конкретная команда/аргументы задаются при создании раннера.
pub struct ProcRunner {
    name: &'static str,
    cfg: ProcConfig,
}

impl ProcRunner {
    pub fn new(cfg: ProcConfig) -> Self {
        Self { name: "proc", cfg }
    }

    pub fn with_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
}

impl Runner for ProcRunner {
    fn name(&self) -> &'static str {
        self.name
    }

    fn supports(&self, spec: &tno_model::CreateSpec) -> bool {
        matches!(spec.kind, TaskKind::Exec)
    }

    fn build_task(
        &self,
        spec: &tno_model::CreateSpec,
        _ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        if self.cfg.program.is_empty() {
            return Err(RunnerError::InvalidSpec("program is empty".into()));
        }

        let cfg = self.cfg.clone();

        // Имя задачи = слот. TaskFn::arc требует &'static str — линкуем строку.
        let name: &'static str = Box::leak(spec.slot.clone().into_boxed_str());

        let task: TaskRef = TaskFn::arc(name, move |ctx: tokio_util::sync::CancellationToken| {
            let cfg = cfg.clone();
            async move {
                use std::process::Stdio;
                use tokio::io::{AsyncBufReadExt, BufReader};

                tracing::trace!(target: "tno.exec.proc", program=%cfg.program, args=?cfg.args, "spawn");

                let mut cmd = tokio::process::Command::new(&cfg.program);
                cmd.args(&cfg.args);

                if let Some(cwd) = &cfg.cwd {
                    cmd.current_dir(cwd);
                }
                for (k, v) in &cfg.env {
                    cmd.env(k, v);
                }

                // ⬇️ Пайпим stdout, stderr оставим в inherit (или тоже пайпни — как хочешь)
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::inherit());

                let mut child = cmd.spawn().map_err(|e| taskvisor::TaskError::Fatal {
                    reason: format!("spawn: {e}"),
                })?;

                // Читаем stdout асинхронно и пишем в логи
                let mut out_lines = {
                    let stdout = child.stdout.take().unwrap(); // у нас piped
                    BufReader::new(stdout).lines()
                };

                let read_stdout = tokio::spawn(async move {
                    while let Ok(Some(line)) = out_lines.next_line().await {
                        //tracing::info!(target: "tno.exec.proc.out", %line);
                    }
                });

                tokio::select! {
                    status = child.wait() => {
                        let status = status
                            .map_err(|e| taskvisor::TaskError::Fatal { reason: format!("wait: {e}") })?;
                        let _ = read_stdout.await; // добираем хвост вывода

                        if !status.success() && cfg.fail_on_non_zero {
                            if let Some(code) = status.code() {
                                return Err(taskvisor::TaskError::Fail { reason: format!("exit code: {code}") });
                            } else {
                                return Err(taskvisor::TaskError::Fail { reason: "terminated by signal".into() });
                            }
                        }

                        tracing::debug!(target: "tno.exec.proc", "exit success");
                        Ok(())
                    }
                    _ = ctx.cancelled() => {
                        tracing::debug!(target: "tno.exec.proc", "cancelled; killing child");
                        let _ = crate::util::kill_graceful(&mut child).await;
                        Err(taskvisor::TaskError::Canceled)
                    }
                }
            }
        });

        Ok(task)
    }
}

#[cfg(feature = "shell")]
pub mod shell;
