//! Runner router that selects an appropriate `Runner` implementation for a given `TaskSpec`.
//!
//! The router checks registered runners in order and delegates task construction
//! to the first one that reports `supports(spec) == true` and matches label constraints (if any).
use std::sync::Arc;

use solti_model::{Labels, TaskKind, TaskSpec};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::{
    error::CoreError,
    runner::{BuildContext, Runner},
};

/// Single runner entry with optional static labels used for routing.
pub struct RunnerEntry {
    /// Concrete runner implementation.
    pub runner: Arc<dyn Runner>,
    /// Static labels attached to this runner (e.g. capacity class, backend tag).
    pub labels: Labels,
}

/// Router that selects an appropriate [`Runner`] for a given [`TaskSpec`].
///
/// Runners are checked in the order they were registered.
/// The first runner whose [`Runner::supports`] method returns `true` and satisfies the [`TaskSpec::runner_selector`] (if any) is used to build the task.
#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<RunnerEntry>,
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

    /// Register a new runner without labels.
    ///
    /// Runners are queried in the order they are registered; the first one that reports `supports(spec) == true` (and matches labels, if any) is used.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) {
        self.runners.push(RunnerEntry {
            runner,
            labels: Labels::default(),
        });
    }

    /// Register a new runner with static labels.
    ///
    /// These labels are used by the router to further narrow down candidates when [`TaskSpec::runner_selector`] is set.
    #[inline]
    pub fn register_with_labels(&mut self, runner: Arc<dyn Runner>, labels: Labels) {
        self.runners.push(RunnerEntry { runner, labels });
    }

    /// Pick the first runner that claims to support the given spec and matches the runner selector.
    ///
    /// Routing rules:
    /// - filter runners by `Runner::supports(spec)`;
    /// - if `spec.runner_selector()` is set, keep only runners whose `labels`
    ///   satisfy all `match_labels` and `match_expressions` requirements;
    /// - pick the first matching entry.
    pub fn pick(&self, spec: &TaskSpec) -> Option<&Arc<dyn Runner>> {
        let selector = spec.runner_selector();

        self.runners
            .iter()
            .filter(|entry| entry.runner.supports(spec))
            .filter(move |entry| {
                match selector {
                    Some(sel) => sel.matches(&entry.labels),
                    None => true,
                }
            })
            .map(|entry| &entry.runner)
            .next()
    }

    /// Build a [`TaskRef`] for the given spec using the selected runner.
    ///
    /// `TaskKind::Embedded` is not routable and must be used with [`SupervisorApi::submit_with_task`](crate::supervisor::SupervisorApi::submit_with_task).
    #[instrument(level = "debug", skip(self, spec), fields(kind = ?spec.kind))]
    pub fn build(&self, spec: &TaskSpec) -> Result<TaskRef, CoreError> {
        trace!(spec = ?spec, "router received spec");

        if matches!(spec.kind, TaskKind::Embedded) {
            return Err(CoreError::NoRunner(
                "TaskKind::Embedded requires submit_with_task()".to_string(),
            ));
        }
        let r = self
            .pick(spec)
            .ok_or_else(|| CoreError::NoRunner(spec.kind.kind().to_string()))?;

        let task = r.build_task(spec, &self.ctx).map_err(CoreError::from)?;
        debug!(runner = r.name(), "runner built task successfully");
        Ok(task)
    }

    /// Returns `true` if at least one registered runner has `label_key == label_value`.
    pub fn contains_label(&self, label_key: &str, label_value: &str) -> bool {
        self.runners
            .iter()
            .any(|e| e.labels.get(label_key) == Some(label_value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunnerError;

    use solti_model::{
        AdmissionPolicy, BackoffPolicy, Flag, JitterPolicy, Labels, RestartPolicy,
        RunnerSelector, TaskEnv,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use taskvisor::{TaskError, TaskFn};
    use tokio_util::sync::CancellationToken;

    struct SubprocessRunnerDummy;

    impl Runner for SubprocessRunnerDummy {
        fn name(&self) -> &'static str {
            "subprocess-only"
        }

        fn supports(&self, spec: &TaskSpec) -> bool {
            matches!(spec.kind, TaskKind::Subprocess { .. })
        }

        fn build_task(
            &self,
            _spec: &TaskSpec,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            let task = TaskFn::arc(
                "test-subprocess-runner",
                |_ctx: CancellationToken| async move { Ok::<(), TaskError>(()) },
            );
            Ok(task)
        }
    }

    fn mk_backoff() -> BackoffPolicy {
        BackoffPolicy {
            jitter: JitterPolicy::Equal,
            first_ms: 1_000,
            max_ms: 5_000,
            factor: 2.0,
        }
    }

    fn mk_spec(kind: TaskKind) -> TaskSpec {
        TaskSpec {
            slot: "test-slot".into(),
            kind,
            timeout: 10_000_u64.into(),
            restart: RestartPolicy::default(),
            backoff: mk_backoff(),
            admission: AdmissionPolicy::DropIfRunning,
            runner_selector: None,
            labels: Labels::default(),
        }
    }

    #[test]
    fn build_fails_for_taskkind_embedded() {
        let router = RunnerRouter::new();
        let spec = mk_spec(TaskKind::Embedded);

        let res = router.build(&spec);

        match res {
            Err(CoreError::NoRunner(msg)) => {
                assert!(
                    msg.contains("TaskKind::Embedded"),
                    "unexpected NoRunner message: {msg}"
                );
            }
            Ok(_) => panic!("expected CoreError::NoRunner for TaskKind::Embedded, got Ok(..)"),
            Err(e) => panic!("expected CoreError::NoRunner for TaskKind::Embedded, got {e:?}"),
        }
    }

    #[test]
    fn build_uses_registered_runner_for_subprocess() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy));

        let spec = mk_spec(TaskKind::Subprocess {
            command: "echo".to_string(),
            args: vec!["hello".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::default(),
        });

        let res = router.build(&spec);

        match res {
            Ok(_task) => {}
            Err(e) => panic!("expected Ok(TaskRef) for subprocess, got error: {e:?}"),
        }
    }

    #[test]
    fn build_fails_when_no_runner_supports_kind() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy));

        let spec = mk_spec(TaskKind::Wasm {
            module: PathBuf::from("mod.wasm"),
            args: Vec::new(),
            env: TaskEnv::default(),
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

    #[test]
    fn pick_respects_runner_selector() {
        struct R1;
        struct R2;

        impl Runner for R1 {
            fn name(&self) -> &'static str {
                "r1"
            }

            fn supports(&self, _spec: &TaskSpec) -> bool {
                true
            }

            fn build_task(
                &self,
                _spec: &TaskSpec,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(
                    "r1-task",
                    |_ctx: CancellationToken| async move { Ok::<(), TaskError>(()) },
                ))
            }
        }

        impl Runner for R2 {
            fn name(&self) -> &'static str {
                "r2"
            }

            fn supports(&self, _spec: &TaskSpec) -> bool {
                true
            }

            fn build_task(
                &self,
                _spec: &TaskSpec,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(
                    "r2-task",
                    |_ctx: CancellationToken| async move { Ok::<(), TaskError>(()) },
                ))
            }
        }

        let mut labels_r1 = Labels::new();
        labels_r1.insert("runner-name", "runner-a");
        let mut labels_r2 = Labels::new();
        labels_r2.insert("runner-name", "runner-b");

        let mut router = RunnerRouter::new();
        router.register_with_labels(Arc::new(R1), labels_r1);
        router.register_with_labels(Arc::new(R2), labels_r2);

        let spec = {
            let base = mk_spec(TaskKind::Subprocess {
                command: "echo".into(),
                args: vec!["hi".into()],
                env: TaskEnv::default(),
                cwd: None,
                fail_on_non_zero: Flag::enabled(),
            });
            base.with_runner_selector(RunnerSelector::from_labels(BTreeMap::from([
                ("runner-name".into(), "runner-b".into()),
            ])))
        };

        let picked = router.pick(&spec).expect("runner should be picked");
        assert_eq!(picked.name(), "r2");
    }
}
