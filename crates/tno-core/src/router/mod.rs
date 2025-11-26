//! Runner router that selects an appropriate `Runner` implementation for a given `CreateSpec`.
//!
//! The router checks registered runners in order and delegates task construction to the first one that reports `supports(spec) == true`.
use std::sync::Arc;

use taskvisor::TaskRef;
use tno_model::{CreateSpec, TaskKind};
use tracing::{debug, instrument, trace};

use crate::{
    error::CoreError,
    runner::{BuildContext, Runner},
};

/// Router that selects an appropriate [`Runner`] for a given [`CreateSpec`].
///
/// Runners are checked in the order they were registered. The first runner whose [`Runner::supports`] method returns `true` is used to build the task.
#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<Arc<dyn Runner>>,
    ctx: BuildContext,
}

impl RunnerRouter {
    /// Create an empty router with a default build context.
    #[inline]
    pub fn new() -> Self {
        Self {
            runners: Vec::new(),
            ctx: BuildContext::default(),
        }
    }

    /// Set a custom build context for all runners managed by this router.
    ///
    /// This is typically used to inject shared dependencies (config, observability, global handles, etc.) into runner instances.
    #[inline]
    pub fn with_context(mut self, ctx: BuildContext) -> Self {
        self.ctx = ctx;
        self
    }

    /// Register a new runner.
    ///
    /// Runners are queried in the order they are registered; the first one that reports `supports(spec) == true` will be used.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) {
        self.runners.push(runner);
    }

    /// Pick the first runner that claims to support the given spec.
    ///
    /// Returns `None` if no runner accepts this spec (e.g. unknown `TaskKind`).
    pub fn pick(&self, spec: &CreateSpec) -> Option<&Arc<dyn Runner>> {
        self.runners.iter().find(|r| r.supports(spec))
    }

    /// Build a [`taskvisor::TaskRef`] for the given spec using the selected runner.
    ///
    /// `TaskKind::None` is not routable and must be used with [`SupervisorApi::submit_with_task`](crate::supervisor::SupervisorApi::submit_with_task).
    #[instrument(level = "debug", skip(self, spec), fields(kind = ?spec.kind, slot = ?spec.slot))]
    pub fn build(&self, spec: &CreateSpec) -> Result<TaskRef, CoreError> {
        trace!(spec = ?spec, "router received spec");

        if matches!(spec.kind, TaskKind::None) {
            return Err(CoreError::NoRunner(
                "TaskKind::None requires submit_with_task()".to_string(),
            ));
        }
        let r = self
            .pick(spec)
            .ok_or_else(|| CoreError::NoRunner(spec.kind.kind().to_string()))?;

        let task = r.build_task(spec, &self.ctx).map_err(CoreError::from)?;
        debug!(runner = r.name(), "runner built task successfully");
        Ok(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunnerError;

    use std::path::PathBuf;
    use taskvisor::{TaskError, TaskFn};
    use tno_model::{
        AdmissionStrategy, BackoffStrategy, Env, Flag, JitterStrategy, RestartStrategy,
    };
    use tokio_util::sync::CancellationToken;

    struct ExecOnlyRunner;

    impl Runner for ExecOnlyRunner {
        fn name(&self) -> &'static str {
            "subprocess-only"
        }

        fn supports(&self, spec: &CreateSpec) -> bool {
            matches!(spec.kind, TaskKind::Subprocess { .. })
        }

        fn build_task(
            &self,
            _spec: &CreateSpec,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            let task = TaskFn::arc(
                "test-subprocess-runner",
                |_ctx: CancellationToken| async move { Ok::<(), TaskError>(()) },
            );
            Ok(task)
        }
    }

    fn mk_backoff() -> BackoffStrategy {
        BackoffStrategy {
            jitter: JitterStrategy::Equal,
            first_ms: 1_000,
            max_ms: 5_000,
            factor: 2.0,
        }
    }

    fn mk_spec(kind: TaskKind) -> CreateSpec {
        CreateSpec {
            slot: "test-slot".to_string(),
            kind,
            timeout_ms: 10_000,
            restart: RestartStrategy::default(),
            backoff: mk_backoff(),
            admission: AdmissionStrategy::DropIfRunning,
        }
    }

    #[test]
    fn build_fails_for_taskkind_none() {
        let router = RunnerRouter::new();
        let spec = mk_spec(TaskKind::None);

        let res = router.build(&spec);

        match res {
            Err(CoreError::NoRunner(msg)) => {
                assert!(
                    msg.contains("TaskKind::None"),
                    "unexpected NoRunner message: {msg}"
                );
            }
            Ok(_) => panic!("expected CoreError::NoRunner for TaskKind::None, got Ok(..)"),
            Err(e) => panic!("expected CoreError::NoRunner for TaskKind::None, got {e:?}"),
        }
    }

    #[test]
    fn build_uses_registered_runner_for_exec() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(ExecOnlyRunner));

        let spec = mk_spec(TaskKind::Subprocess {
            command: "echo".to_string(),
            args: vec!["hello".into()],
            env: Env::default(),
            cwd: None,
            fail_on_non_zero: Flag::default(),
        });

        let res = router.build(&spec);

        match res {
            Ok(_task) => {
                // ok
            }
            Err(e) => panic!("expected Ok(TaskRef) for subprocess, got error: {e:?}"),
        }
    }

    #[test]
    fn build_fails_when_no_runner_supports_kind() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(ExecOnlyRunner));

        let spec = mk_spec(TaskKind::Wasm {
            module: PathBuf::from("mod.wasm"),
            args: Vec::new(),
            env: Env::default(),
        });

        let res = router.build(&spec);

        match res {
            Err(CoreError::NoRunner(kind)) => {
                assert_eq!(kind, "wasm", "expected NoRunner(\"wasm\")");
            }
            Ok(_) => panic!("expected CoreError::NoRunner for wasm, got Ok(..)"),
            Err(e) => panic!("expected CoreError::NoRunner for wasm, got {e:?}"),
        }
    }
}
