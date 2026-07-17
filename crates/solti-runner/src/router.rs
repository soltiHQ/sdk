//! # Runner router.
//!
//! [`RunnerRouter`] selects the first registered [`Runner`](crate::Runner) that:
//! 1. returns `true` from [`supports`](crate::Runner::supports) for the spec;
//! 2. matches the [`RunnerSelector`](solti_model::RunnerSelector), if the spec has one.
//!
//! Runners are checked in registration order.
//!
//! See the [crate root](crate) for architecture overview.
use std::sync::Arc;

use solti_model::{Labels, TaskKind, TaskSpec};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::error::RunnerError;
use crate::runner::Runner;
use crate::{context::BuildContext, output::OutputRegistry};

/// Single runner entry with optional static labels used for routing.
struct RunnerEntry {
    /// Concrete runner implementation.
    runner: Arc<dyn Runner>,
    /// Static labels attached to this runner (e.g. capacity class, backend tag).
    labels: Labels,
}

/// Router that selects a [`Runner`] for a [`TaskSpec`].
///
/// Runners are checked in the order they were registered.
/// The first runner whose [`Runner::supports`] method returns `true` and satisfies the optional [`TaskSpec::runner_selector`] is used.
///
/// ## Notes
///
/// - `TaskKind::Embedded` is not routable. Submit it directly with `SupervisorApi::submit_with_task`.
/// - Default [`BuildContext`] uses empty env and [`NoOpMetrics`](crate::NoOpMetrics).
///
/// ## Also
///
/// - [`Runner`] - trait that concrete executors implement.
/// - [`BuildContext`] - shared dependencies for all runners.
/// - [`RunnerError::NoRunner`](crate::RunnerError::NoRunner) - returned when no runner matches.
///
/// ## Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use solti_runner::RunnerRouter;
/// # use solti_runner::{BuildContext, Runner, RunnerError};
/// # use solti_model::{TaskKind, TaskSpec};
/// # use taskvisor::TaskRef;
/// # struct MyRunner;
/// # impl Runner for MyRunner {
/// #     fn name(&self) -> &'static str { "my-runner" }
/// #     fn supports(&self, spec: &TaskSpec) -> bool { matches!(spec.kind(), TaskKind::Subprocess(_)) }
/// #     fn build_task(&self, _s: &TaskSpec, _c: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
/// # }
/// # fn demo(spec: &TaskSpec) -> Result<(), RunnerError> {
/// let mut router = RunnerRouter::new();
/// router.register(Arc::new(MyRunner));
///
/// let task_ref = router.build(spec)?; // picks the first runner that supports `spec`
/// # let _ = task_ref;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<RunnerEntry>,
    ctx: BuildContext,
}

impl RunnerRouter {
    /// Create an empty router with a default build context.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::RunnerRouter;
    ///
    /// let router = RunnerRouter::new();
    /// assert!(!router.contains_label("runner", "default"));
    /// ```
    #[inline]
    pub fn new() -> Self {
        Self {
            runners: Vec::new(),
            ctx: BuildContext::default(),
        }
    }

    /// Set a custom build context for all runners managed by this router.
    ///
    /// Use this to inject shared env, metrics, or output registry handles into every runner.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::{BuildContext, RunnerRouter};
    ///
    /// let ctx = BuildContext::default();
    /// let router = RunnerRouter::new().with_context(ctx);
    /// assert!(!router.contains_label("kind", "subprocess"));
    /// ```
    #[inline]
    pub fn with_context(mut self, ctx: BuildContext) -> Self {
        self.ctx = ctx;
        self
    }

    /// Replace the output registry in the existing build context.
    ///
    /// Environment and metrics configuration are preserved. Supervisor
    /// constructors use this to keep runner output and API live-tail streams on
    /// the same registry.
    #[inline]
    pub fn with_output_registry(mut self, registry: Arc<OutputRegistry>) -> Self {
        self.ctx = self.ctx.with_output_registry(registry);
        self
    }

    /// Return the output registry injected into runners built by this router.
    #[inline]
    pub fn output_registry(&self) -> &Arc<OutputRegistry> {
        self.ctx.output_registry()
    }

    /// Register a new runner without labels.
    ///
    /// Runners are queried in the order they are registered.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use solti_runner::RunnerRouter;
    /// # use solti_runner::{BuildContext, Runner, RunnerError};
    /// # use solti_model::TaskSpec;
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, _spec: &TaskSpec) -> bool { true }
    /// #     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
    /// # }
    /// let mut router = RunnerRouter::new();
    /// router.register(Arc::new(MyRunner));
    /// ```
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) {
        self.runners.push(RunnerEntry {
            runner,
            labels: Labels::default(),
        });
    }

    /// Register a new runner with static labels.
    ///
    /// These labels are used only when [`TaskSpec::runner_selector`] is set.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use solti_model::Labels;
    /// use solti_runner::RunnerRouter;
    /// # use solti_runner::{BuildContext, Runner, RunnerError};
    /// # use solti_model::TaskSpec;
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "gpu-runner" }
    /// #     fn supports(&self, _spec: &TaskSpec) -> bool { true }
    /// #     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
    /// # }
    /// let mut labels = Labels::new();
    /// labels.insert("gpu", "true");
    ///
    /// let mut router = RunnerRouter::new();
    /// router.register_with_labels(Arc::new(MyRunner), labels);
    /// assert!(router.contains_label("gpu", "true"));
    /// ```
    #[inline]
    pub fn register_with_labels(&mut self, runner: Arc<dyn Runner>, labels: Labels) {
        self.runners.push(RunnerEntry { runner, labels });
    }

    /// Pick the first runner that supports the spec and matches its selector.
    ///
    /// Routing rules:
    /// - filter runners by `Runner::supports(spec)`;
    /// - if `spec.runner_selector()` is set, keep only runners whose labels satisfy all selector requirements;
    /// - pick the first matching entry.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use solti_model::{TaskKind, TaskSpec};
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, _spec: &TaskSpec) -> bool { true }
    /// #     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
    /// # }
    /// let spec = TaskSpec::builder("slot-a", TaskKind::Embedded, 1_000u64).build()?;
    /// let mut router = RunnerRouter::new();
    /// router.register(Arc::new(MyRunner));
    ///
    /// let picked = router.pick(&spec).unwrap();
    /// assert_eq!(picked.name(), "my-runner");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn pick(&self, spec: &TaskSpec) -> Option<&Arc<dyn Runner>> {
        let selector = spec.runner_selector();

        let mut matching = self.runners.iter().filter(|entry| {
            entry.runner.supports(spec) && selector.is_none_or(|sel| sel.matches(&entry.labels))
        });

        let first = matching.next()?;
        if matching.next().is_some() {
            debug!(
                slot = %spec.slot(),
                runner = first.runner.name(),
                "multiple runners match this spec; using the first registered (registration order is significant)"
            );
        }
        Some(&first.runner)
    }

    /// Build a [`TaskRef`] for the given spec using the selected runner.
    ///
    /// `TaskKind::Embedded` is not routable and must be used with `SupervisorApi::submit_with_task`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec};
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, spec: &TaskSpec) -> bool { matches!(spec.kind(), TaskKind::Subprocess(_)) }
    /// #     fn build_task(&self, spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
    /// #         let id = self.build_run_id(spec.slot().as_ref());
    /// #         Ok(TaskFn::arc(id.into_name(), |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) }))
    /// #     }
    /// # }
    /// let kind = TaskKind::Subprocess(SubprocessSpec::new(
    ///     SubprocessMode::Command { command: "echo".into(), args: vec!["hi".into()] },
    ///     TaskEnv::default(),
    ///     None,
    ///     Flag::enabled(),
    /// ));
    /// let spec = TaskSpec::builder("slot-a", kind, 1_000u64).build()?;
    ///
    /// let mut router = RunnerRouter::new();
    /// router.register(Arc::new(MyRunner));
    ///
    /// let task = router.build(&spec)?;
    /// # let _ = task;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[instrument(level = "debug", skip(self, spec), fields(kind = ?spec.kind()))]
    pub fn build(&self, spec: &TaskSpec) -> Result<TaskRef, RunnerError> {
        trace!(spec = ?spec, "router received spec");

        if matches!(spec.kind(), TaskKind::Embedded) {
            return Err(RunnerError::NoRunner(
                "TaskKind::Embedded requires submit_with_task()".to_string(),
            ));
        }
        let r = self
            .pick(spec)
            .ok_or_else(|| RunnerError::NoRunner(spec.kind().kind().to_string()))?;

        let task = r.build_task(spec, &self.ctx)?;
        debug!(runner = r.name(), "runner built task successfully");
        Ok(task)
    }

    /// Returns `true` if at least one registered runner has `label_key == label_value`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use solti_model::Labels;
    /// # use solti_model::TaskSpec;
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "gpu-runner" }
    /// #     fn supports(&self, _spec: &TaskSpec) -> bool { true }
    /// #     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
    /// # }
    /// let mut labels = Labels::new();
    /// labels.insert("gpu", "true");
    ///
    /// let mut router = RunnerRouter::new();
    /// router.register_with_labels(Arc::new(MyRunner), labels);
    ///
    /// assert!(router.contains_label("gpu", "true"));
    /// ```
    pub fn contains_label(&self, label_key: &str, label_value: &str) -> bool {
        self.runners
            .iter()
            .any(|e| e.labels.get(label_key) == Some(label_value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunnerError;

    use solti_model::{
        AdmissionPolicy, BackoffPolicy, Flag, JitterPolicy, Labels, RunnerSelector, SubprocessMode,
        SubprocessSpec, TaskEnv, WasmSpec,
    };
    use std::path::PathBuf;
    use taskvisor::{TaskContext, TaskError, TaskFn};

    struct SubprocessRunnerDummy;

    impl Runner for SubprocessRunnerDummy {
        fn name(&self) -> &'static str {
            "subprocess-only"
        }

        fn supports(&self, spec: &TaskSpec) -> bool {
            matches!(spec.kind(), TaskKind::Subprocess(_))
        }

        fn build_task(
            &self,
            _spec: &TaskSpec,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            let task = TaskFn::arc("test-subprocess-runner", |_ctx: TaskContext| async move {
                Ok::<(), TaskError>(())
            });
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
        TaskSpec::builder("test-slot", kind, 10_000_u64)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec")
    }

    #[test]
    fn build_fails_for_taskkind_embedded() {
        let router = RunnerRouter::new();
        let spec = mk_spec(TaskKind::Embedded);

        let res = router.build(&spec);

        match res {
            Err(RunnerError::NoRunner(msg)) => {
                assert!(
                    msg.contains("TaskKind::Embedded"),
                    "unexpected NoRunner message: {msg}"
                );
            }
            Ok(_) => panic!("expected RunnerError::NoRunner for TaskKind::Embedded, got Ok(..)"),
            Err(e) => panic!("expected RunnerError::NoRunner for TaskKind::Embedded, got {e:?}"),
        }
    }

    #[test]
    fn output_registry_can_be_rebound_without_rebuilding_the_router() {
        let registry = Arc::new(OutputRegistry::new(32));
        let router = RunnerRouter::new().with_output_registry(Arc::clone(&registry));

        assert!(Arc::ptr_eq(router.output_registry(), &registry));
    }

    #[test]
    fn build_uses_registered_runner_for_subprocess() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy));

        let spec = mk_spec(TaskKind::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".to_string(),
                args: vec!["hello".into()],
            },
            TaskEnv::default(),
            None,
            Flag::default(),
        )));

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

        let spec = mk_spec(TaskKind::Wasm(WasmSpec::new(
            PathBuf::from("mod.wasm"),
            Vec::new(),
            TaskEnv::default(),
        )));

        let res = router.build(&spec);

        match res {
            Err(RunnerError::NoRunner(kind)) => {
                assert_eq!(kind, "wasm", "expected NoRunner(\"wasm\")");
            }
            Ok(_) => panic!("expected RunnerError::NoRunner for wasm, got Ok(..)"),
            Err(e) => panic!("expected RunnerError::NoRunner for wasm, got {e:?}"),
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
                Ok(TaskFn::arc("r1-task", |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
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
                Ok(TaskFn::arc("r2-task", |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
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
            let base = mk_spec(TaskKind::Subprocess(SubprocessSpec::new(
                SubprocessMode::Command {
                    command: "echo".into(),
                    args: vec!["hi".into()],
                },
                TaskEnv::default(),
                None,
                Flag::enabled(),
            )));
            let mut match_labels = Labels::new();
            match_labels.insert("runner-name", "runner-b");
            base.with_runner_selector(RunnerSelector::from_labels(match_labels))
        };

        let picked = router.pick(&spec).expect("runner should be picked");
        assert_eq!(picked.name(), "r2");
    }
}
