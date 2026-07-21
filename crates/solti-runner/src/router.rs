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

use solti_model::{Labels, Task, TaskWorkload};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::error::RunnerError;
use crate::runner::Runner;
use crate::{context::BuildContext, output::OutputPublisherHandle};

/// Single runner entry with optional static labels used for routing.
struct RunnerEntry {
    /// Concrete runner implementation.
    runner: Arc<dyn Runner>,
    /// Static labels attached to this runner (e.g. capacity class, backend tag).
    labels: Labels,
}

/// Router that selects a [`Runner`] for a [`Task`].
///
/// Runners are checked in the order they were registered.
/// The first runner whose [`Runner::supports`] method returns `true` and satisfies the optional [`TaskSpec::runner_selector`](solti_model::TaskSpec::runner_selector) is used.
///
/// ## Notes
///
/// - [`TaskWorkload::Embedded`] is not routable. Pass it with its prebuilt task to `SupervisorApi::create_with_task` or `SupervisorApi::apply_with_task`.
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
/// # use solti_model::{Task, TaskWorkload};
/// # use taskvisor::TaskRef;
/// # struct MyRunner;
/// # impl Runner for MyRunner {
/// #     fn name(&self) -> &'static str { "my-runner" }
/// #     fn supports(&self, workload: &TaskWorkload) -> bool { matches!(workload, TaskWorkload::Subprocess(_)) }
/// #     fn build_task(&self, _task: &Task, _c: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
/// # }
/// # fn demo(resource: &Task) -> Result<(), RunnerError> {
/// let mut router = RunnerRouter::new();
/// router.register(Arc::new(MyRunner));
///
/// let task_ref = router.build(resource)?; // picks the first runner that supports the workload
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
    /// Use this to inject shared env, metrics, or an output producer capability into every runner.
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

    /// Replace the output producer capability in the existing build context.
    ///
    /// The environment and metrics configuration are preserved.
    #[inline]
    pub fn with_output_publisher(mut self, publisher: OutputPublisherHandle) -> Self {
        self.ctx = self.ctx.with_output_publisher(publisher);
        self
    }

    /// Return the output producer capability injected into built runners.
    #[inline]
    pub fn output_publisher(&self) -> &OutputPublisherHandle {
        self.ctx.output_publisher()
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
    /// # use solti_model::{Task, TaskWorkload};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, _workload: &TaskWorkload) -> bool { true }
    /// #     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
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
    /// These labels are used only when [`TaskSpec::runner_selector`](solti_model::TaskSpec::runner_selector) is set.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use solti_model::Labels;
    /// use solti_runner::RunnerRouter;
    /// # use solti_runner::{BuildContext, Runner, RunnerError};
    /// # use solti_model::{Task, TaskWorkload};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "gpu-runner" }
    /// #     fn supports(&self, _workload: &TaskWorkload) -> bool { true }
    /// #     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
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

    /// Pick the first runner that supports the task workload and matches its selector.
    ///
    /// Routing rules:
    /// - filter runners by `Runner::supports(task.spec().workload())`;
    /// - if `task.spec().runner_selector()` is set, keep only runners whose labels satisfy all selector requirements;
    /// - pick the first matching entry.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use solti_model::{Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskWorkload};
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, _workload: &TaskWorkload) -> bool { true }
    /// #     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
    /// # }
    /// let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
    ///     SubprocessMode::Command { command: "echo".into(), args: vec![] },
    ///     TaskEnv::default(),
    ///     None,
    ///     Flag::enabled(),
    /// ));
    /// let spec = solti_model::TaskSpec::builder("slot-a", workload, 1_000u64).build()?;
    /// let task = Task::new("task-a", spec)?;
    /// let mut router = RunnerRouter::new();
    /// router.register(Arc::new(MyRunner));
    ///
    /// let picked = router.pick(&task).unwrap();
    /// assert_eq!(picked.name(), "my-runner");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn pick(&self, task: &Task) -> Option<&Arc<dyn Runner>> {
        let selector = task.spec().runner_selector();
        let workload = task.spec().workload();
        if matches!(workload, TaskWorkload::Embedded(_)) {
            return None;
        }

        let mut matching = self.runners.iter().filter(|entry| {
            entry.runner.supports(workload) && selector.is_none_or(|sel| sel.matches(&entry.labels))
        });

        let first = matching.next()?;
        if matching.next().is_some() {
            debug!(
                task = %task.name(),
                slot = %task.slot(),
                api_version = workload.api_version(),
                kind = workload.kind(),
                runner = first.runner.name(),
                "multiple runners match this spec; using the first registered (registration order is significant)"
            );
        }
        Some(&first.runner)
    }

    /// Build a [`TaskRef`] for the given task using the selected runner.
    ///
    /// [`TaskWorkload::Embedded`] is not routable and must be used with `SupervisorApi::create_with_task` or `SupervisorApi::apply_with_task`.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use solti_model::{Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskSpec, TaskWorkload};
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "my-runner" }
    /// #     fn supports(&self, workload: &TaskWorkload) -> bool { matches!(workload, TaskWorkload::Subprocess(_)) }
    /// #     fn build_task(&self, task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
    /// #         let id = self.build_run_id(task.slot().as_ref());
    /// #         Ok(TaskFn::arc(id.into_name(), |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) }))
    /// #     }
    /// # }
    /// let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
    ///     SubprocessMode::Command { command: "echo".into(), args: vec!["hi".into()] },
    ///     TaskEnv::default(),
    ///     None,
    ///     Flag::enabled(),
    /// ));
    /// let spec = TaskSpec::builder("slot-a", workload, 1_000u64).build()?;
    /// let task = Task::new("task-a", spec)?;
    ///
    /// let mut router = RunnerRouter::new();
    /// router.register(Arc::new(MyRunner));
    ///
    /// let task_ref = router.build(&task)?;
    /// # let _ = task_ref;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[instrument(
        level = "debug",
        skip(self, task),
        fields(
            task = %task.name(),
            api_version = task.spec().workload().api_version(),
            kind = task.spec().workload().kind()
        )
    )]
    pub fn build(&self, task: &Task) -> Result<TaskRef, RunnerError> {
        trace!(task = ?task, "router received task");

        let workload = task.spec().workload();
        if matches!(workload, TaskWorkload::Embedded(_)) {
            return Err(RunnerError::NoRunner(
                "solti.io/v1/Embedded requires create_with_task() or apply_with_task()".to_string(),
            ));
        }
        let r = self.pick(task).ok_or_else(|| {
            RunnerError::NoRunner(format!("{}/{}", workload.api_version(), workload.kind()))
        })?;

        let task = r.build_task(task, &self.ctx)?;
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
    /// # use solti_model::{Task, TaskWorkload};
    /// # use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
    /// # use taskvisor::TaskRef;
    /// # struct MyRunner;
    /// # impl Runner for MyRunner {
    /// #     fn name(&self) -> &'static str { "gpu-runner" }
    /// #     fn supports(&self, _workload: &TaskWorkload) -> bool { true }
    /// #     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
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
        AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, Labels, RunnerSelector,
        SubprocessMode, SubprocessSpec, TaskEnv, TaskSpec, WasmSpec,
    };
    use std::path::PathBuf;
    use taskvisor::{TaskContext, TaskError, TaskFn};

    struct SubprocessRunnerDummy;

    impl Runner for SubprocessRunnerDummy {
        fn name(&self) -> &'static str {
            "subprocess-only"
        }

        fn supports(&self, workload: &TaskWorkload) -> bool {
            matches!(workload, TaskWorkload::Subprocess(_))
        }

        fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
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

    fn mk_task(workload: TaskWorkload) -> Task {
        let spec = TaskSpec::builder("test-slot", workload, 10_000_u64)
            .backoff(mk_backoff())
            .admission(AdmissionPolicy::DropIfRunning)
            .build()
            .expect("valid spec");
        Task::new("test-task", spec).expect("valid task")
    }

    #[test]
    fn build_fails_for_embedded_workload() {
        let router = RunnerRouter::new();
        let task = mk_task(TaskWorkload::Embedded(
            EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
        ));

        let res = router.build(&task);

        match res {
            Err(RunnerError::NoRunner(msg)) => {
                assert!(
                    msg.contains("Embedded"),
                    "unexpected NoRunner message: {msg}"
                );
            }
            Ok(_) => panic!("expected RunnerError::NoRunner for Embedded workload, got Ok(..)"),
            Err(e) => panic!("expected RunnerError::NoRunner for Embedded workload, got {e:?}"),
        }
    }

    #[test]
    fn pick_never_routes_embedded_workload() {
        struct AcceptsEverything;

        impl Runner for AcceptsEverything {
            fn name(&self) -> &'static str {
                "accepts-everything"
            }

            fn supports(&self, _workload: &TaskWorkload) -> bool {
                true
            }

            fn build_task(
                &self,
                _task: &Task,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                unreachable!("Embedded workloads must not reach a runner")
            }
        }

        let mut router = RunnerRouter::new();
        router.register(Arc::new(AcceptsEverything));

        assert!(
            router
                .pick(&mk_task(TaskWorkload::Embedded(
                    EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
                )))
                .is_none()
        );
    }

    #[test]
    fn output_publisher_can_be_rebound_without_rebuilding_the_router() {
        struct Publisher;
        impl crate::OutputPublisher for Publisher {
            fn sink_for(
                &self,
                _task_name: &solti_model::TaskId,
                generation: u64,
                attempt: u32,
            ) -> Option<crate::OutputSink> {
                Some(crate::OutputSink::new(generation, attempt, |_| {}))
            }
        }

        let publisher: crate::OutputPublisherHandle = Arc::new(Publisher);
        let router = RunnerRouter::new().with_output_publisher(Arc::clone(&publisher));

        assert!(Arc::ptr_eq(router.output_publisher(), &publisher));
    }

    #[test]
    fn build_uses_registered_runner_for_subprocess() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy));

        let task = mk_task(TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".to_string(),
                args: vec!["hello".into()],
            },
            TaskEnv::default(),
            None,
            Flag::default(),
        )));

        let res = router.build(&task);

        match res {
            Ok(_task) => {}
            Err(e) => panic!("expected Ok(TaskRef) for subprocess, got error: {e:?}"),
        }
    }

    #[test]
    fn build_fails_when_no_runner_supports_kind() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy));

        let task = mk_task(TaskWorkload::Wasm(WasmSpec::new(
            PathBuf::from("mod.wasm"),
            Vec::new(),
            TaskEnv::default(),
        )));

        let res = router.build(&task);

        match res {
            Err(RunnerError::NoRunner(kind)) => {
                assert_eq!(kind, "solti.io/v1/Wasm");
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

            fn supports(&self, _workload: &TaskWorkload) -> bool {
                true
            }

            fn build_task(
                &self,
                _task: &Task,
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

            fn supports(&self, _workload: &TaskWorkload) -> bool {
                true
            }

            fn build_task(
                &self,
                _task: &Task,
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

        let task = {
            let base = mk_task(TaskWorkload::Subprocess(SubprocessSpec::new(
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
            let (_, metadata, spec, status) = base.into_parts();
            Task::from_parts(
                solti_model::TypeMeta::task(),
                metadata,
                spec.with_runner_selector(RunnerSelector::from_labels(match_labels)),
                status,
            )
            .unwrap()
        };

        let picked = router.pick(&task).expect("runner should be picked");
        assert_eq!(picked.name(), "r2");
    }

    #[test]
    fn application_defined_gvk_routes_through_registered_runner() {
        struct ImageResizeRunner;

        impl Runner for ImageResizeRunner {
            fn name(&self) -> &'static str {
                "image-resize"
            }

            fn supports(&self, workload: &TaskWorkload) -> bool {
                workload.api_version() == "tasks.example.io/v1" && workload.kind() == "ImageResize"
            }

            fn build_task(
                &self,
                _task: &Task,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(
                    "image-resize-run",
                    |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) },
                ))
            }
        }

        let workload = TaskWorkload::Extension(
            solti_model::ExtensionWorkload::new(
                "tasks.example.io/v1",
                "ImageResize",
                serde_json::json!({ "width": 1280 }),
            )
            .unwrap(),
        );
        let task = mk_task(workload);
        let mut router = RunnerRouter::new();
        router.register(Arc::new(ImageResizeRunner));

        let built = router.build(&task).unwrap();
        assert_eq!(built.name(), "image-resize-run");
    }
}
