//! # Runner router
//!
//! [`RunnerRouter`] selects runners by workload GVK and optional
//! [`LabelSelector`](solti_model::LabelSelector).
//!
//! Runners are checked in registration order.
//!
//! See the [crate root](crate) for architecture overview.
use std::sync::Arc;

use solti_model::{AgentCapabilities, Labels, RunnerCapability, Task, TaskWorkload};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::error::RouterError;
use crate::runner::Runner;
use crate::{context::BuildContext, id::make_run_id, output::OutputPublisherHandle};

/// Single runner entry with optional static labels used for routing.
struct RunnerEntry {
    /// Concrete runner implementation.
    runner: Arc<dyn Runner>,
    /// Immutable routing and discovery metadata captured at registration.
    capability: RunnerCapability,
}

/// Router that selects a [`Runner`] for a [`Task`].
///
/// Runners are checked in the order they were registered.
/// The first matching GVK and selector wins.
///
/// ## Rules
///
/// - Runner names are unique.
/// - Labels are used only by `runnerSelector`.
/// - Embedded workloads are not routed.
/// - The router allocates the [`RunId`](crate::RunId).
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
    /// Use it to inject shared env, metrics, and output publishing.
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

    /// Register a new runner without labels.
    ///
    /// Runners are queried in the order they are registered.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] if the name is already registered,
    /// or [`RouterError::InvalidCapability`] when the runner declaration is invalid.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) -> Result<(), RouterError> {
        self.register_with_labels(runner, Labels::default())
    }

    /// Register a new runner with static labels.
    ///
    /// Labels are read only when the task has a
    /// [`runnerSelector`](solti_model::TaskSpec::runner_selector).
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] if the name is already registered,
    /// [`RouterError::InvalidLabels`] when a label violates the model rules, or
    /// [`RouterError::InvalidCapability`] when the runner declaration is invalid.
    #[inline]
    pub fn register_with_labels(
        &mut self,
        runner: Arc<dyn Runner>,
        labels: Labels,
    ) -> Result<(), RouterError> {
        let runner_name = runner.name().to_owned();
        if self
            .runners
            .iter()
            .any(|entry| entry.capability.name() == runner_name)
        {
            return Err(RouterError::DuplicateRunner { name: runner_name });
        }
        labels
            .validate()
            .map_err(|source| RouterError::InvalidLabels {
                runner: runner_name.clone(),
                source,
            })?;
        let capability =
            RunnerCapability::new(runner_name.clone(), labels, runner.workload_types()).map_err(
                |source| RouterError::InvalidCapability {
                    runner: runner_name,
                    source,
                },
            )?;
        self.runners.push(RunnerEntry { runner, capability });
        Ok(())
    }

    /// Return an owned immutable snapshot of registered runner capabilities.
    ///
    /// Runners remain in routing priority order. Each entry contains the exact
    /// GVK declaration and labels captured during registration.
    pub fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::new(
            self.runners
                .iter()
                .map(|entry| entry.capability.clone())
                .collect(),
        )
        .expect("RunnerRouter registration preserves unique runner names")
    }

    /// Pick the first runner whose GVK and labels match the task.
    ///
    /// Routing rules:
    /// - filter runners by workload type metadata;
    /// - apply `runnerSelector` to registered labels;
    /// - pick the first matching entry.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::EmbeddedWorkload`] for Embedded workloads and
    /// [`RouterError::NoRunner`] when no entry matches.
    pub fn pick(&self, task: &Task) -> Result<&dyn Runner, RouterError> {
        Ok(self.pick_entry(task)?.runner.as_ref())
    }

    fn pick_entry(&self, task: &Task) -> Result<&RunnerEntry, RouterError> {
        let selector = task.spec().runner_selector();
        let workload = task.spec().workload();
        if matches!(workload, TaskWorkload::Embedded(_)) {
            return Err(RouterError::EmbeddedWorkload);
        }
        let workload_type = workload.type_meta();

        let mut matching = self.runners.iter().filter(|entry| {
            entry.capability.workload_types().contains(&workload_type)
                && selector.is_none_or(|sel| sel.matches(entry.capability.labels()))
        });

        let first = matching.next().ok_or_else(|| RouterError::NoRunner {
            api_version: workload_type.api_version().to_owned(),
            kind: workload_type.kind().to_owned(),
        })?;
        if matching.next().is_some() {
            debug!(
                task = %task.name(),
                slot = %task.slot(),
                api_version = workload.api_version(),
                kind = workload.kind(),
                runner = first.capability.name(),
                "multiple runners match this spec; using the first registered (registration order is significant)"
            );
        }
        Ok(first)
    }

    /// Build a [`TaskRef`] for the given task using the selected runner.
    ///
    /// The router allocates one [`RunId`](crate::RunId), passes it to the selected
    /// runner, and checks the returned task name.
    ///
    /// # Errors
    ///
    /// Returns a [`RouterError`] when routing or task construction fails.
    #[instrument(
        level = "debug",
        skip(self, task),
        fields(
            task = %task.name(),
            api_version = task.spec().workload().api_version(),
            kind = task.spec().workload().kind()
        )
    )]
    pub fn build(&self, task: &Task) -> Result<TaskRef, RouterError> {
        trace!(task = ?task, "router received task");

        let entry = self.pick_entry(task)?;
        let runner = entry.runner.as_ref();
        let runner_name = entry.capability.name().to_owned();
        let run_id = make_run_id(&runner_name, task.slot().as_str());
        let task_ref = runner
            .build_task(task, &run_id, &self.ctx)
            .map_err(|source| RouterError::Build {
                runner: runner_name.clone(),
                source,
            })?;
        if task_ref.name() != run_id.name() {
            return Err(RouterError::RunIdMismatch {
                runner: runner_name,
                expected: run_id.into_name(),
                actual: task_ref.name().to_owned(),
            });
        }
        debug!(
            runner = runner_name,
            run_id = task_ref.name(),
            "runner built task successfully"
        );
        Ok(task_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RunId, RunnerError};

    use solti_model::{
        AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, LabelSelector, Labels,
        SubprocessMode, SubprocessSpec, TaskEnv, TaskSpec, WasmSpec, WorkloadTypeMeta,
    };
    use std::path::PathBuf;
    use taskvisor::{TaskContext, TaskError, TaskFn};

    struct SubprocessRunnerDummy;

    impl Runner for SubprocessRunnerDummy {
        fn name(&self) -> &str {
            "subprocess-only"
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            vec![
                WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                    .expect("built-in workload GVK"),
            ]
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            let task = TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                Ok::<(), TaskError>(())
            });
            Ok(task)
        }
    }

    struct DeclaredRunner {
        name: &'static str,
        workload_types: Vec<WorkloadTypeMeta>,
    }

    impl Runner for DeclaredRunner {
        fn name(&self) -> &str {
            self.name
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            self.workload_types.clone()
        }

        fn build_task(
            &self,
            _task: &Task,
            run_id: &RunId,
            _ctx: &BuildContext,
        ) -> Result<TaskRef, RunnerError> {
            Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                Ok::<(), TaskError>(())
            }))
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
            Err(RouterError::EmbeddedWorkload) => {}
            Ok(_) => panic!("expected RouterError::EmbeddedWorkload, got Ok(..)"),
            Err(e) => panic!("expected RouterError::EmbeddedWorkload, got {e:?}"),
        }
    }

    #[test]
    fn pick_never_routes_embedded_workload() {
        struct AcceptsEverything;

        impl Runner for AcceptsEverything {
            fn name(&self) -> &str {
                "accepts-everything"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                        .expect("built-in workload GVK"),
                ]
            }

            fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                unreachable!("Embedded workloads must not reach a runner")
            }
        }

        let mut router = RunnerRouter::new();
        router.register(Arc::new(AcceptsEverything)).unwrap();

        assert!(
            router
                .pick(&mk_task(TaskWorkload::Embedded(
                    EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
                )))
                .is_err()
        );
    }

    #[test]
    fn build_uses_registered_runner_for_subprocess() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy)).unwrap();

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
        router.register(Arc::new(SubprocessRunnerDummy)).unwrap();

        let task = mk_task(TaskWorkload::Wasm(WasmSpec::new(
            PathBuf::from("mod.wasm"),
            Vec::new(),
            TaskEnv::default(),
        )));

        let res = router.build(&task);

        match res {
            Err(RouterError::NoRunner { api_version, kind }) => {
                assert_eq!(api_version, "solti.io/v1");
                assert_eq!(kind, "Wasm");
            }
            Ok(_) => panic!("expected RouterError::NoRunner for wasm, got Ok(..)"),
            Err(e) => panic!("expected RouterError::NoRunner for wasm, got {e:?}"),
        }
    }

    #[test]
    fn pick_respects_runner_selector() {
        struct R1;
        struct R2;

        impl Runner for R1 {
            fn name(&self) -> &str {
                "r1"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                        .expect("built-in workload GVK"),
                ]
            }

            fn build_task(
                &self,
                _task: &Task,
                run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
            }
        }

        impl Runner for R2 {
            fn name(&self) -> &str {
                "r2"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                        .expect("built-in workload GVK"),
                ]
            }

            fn build_task(
                &self,
                _task: &Task,
                run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
            }
        }

        let mut labels_r1 = Labels::new();
        labels_r1.insert("runner-name", "runner-a");
        let mut labels_r2 = Labels::new();
        labels_r2.insert("runner-name", "runner-b");

        let mut router = RunnerRouter::new();
        router
            .register_with_labels(Arc::new(R1), labels_r1)
            .unwrap();
        router
            .register_with_labels(Arc::new(R2), labels_r2)
            .unwrap();

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
                spec.with_runner_selector(LabelSelector::from_labels(match_labels)),
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
            fn name(&self) -> &str {
                "image-resize"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new("tasks.example.io/v1", "ImageResize")
                        .expect("extension workload GVK"),
                ]
            }

            fn build_task(
                &self,
                _task: &Task,
                run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
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
        router.register(Arc::new(ImageResizeRunner)).unwrap();

        let built = router.build(&task).unwrap();
        assert!(built.name().starts_with("image-resize-test-slot-"));
    }

    #[test]
    fn duplicate_runner_name_is_rejected() {
        let mut router = RunnerRouter::new();
        router.register(Arc::new(SubprocessRunnerDummy)).unwrap();

        let error = router
            .register(Arc::new(SubprocessRunnerDummy))
            .unwrap_err();

        assert!(matches!(error, RouterError::DuplicateRunner { .. }));
    }

    #[test]
    fn invalid_runner_labels_are_rejected_without_registration() {
        let mut labels = Labels::new();
        labels.insert("invalid label key", "value");
        let mut router = RunnerRouter::new();

        let error = router
            .register_with_labels(Arc::new(SubprocessRunnerDummy), labels)
            .unwrap_err();

        assert!(matches!(error, RouterError::InvalidLabels { .. }));
        router.register(Arc::new(SubprocessRunnerDummy)).unwrap();
    }

    #[test]
    fn invalid_runner_capability_is_rejected_without_registration() {
        let invalid = [
            DeclaredRunner {
                name: "empty-workloads",
                workload_types: Vec::new(),
            },
            DeclaredRunner {
                name: "duplicate-workload",
                workload_types: vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess").unwrap(),
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess").unwrap(),
                ],
            },
            DeclaredRunner {
                name: "embedded-workload",
                workload_types: vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Embedded").unwrap(),
                ],
            },
            DeclaredRunner {
                name: "invalid/name",
                workload_types: vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess").unwrap(),
                ],
            },
        ];
        let mut router = RunnerRouter::new();

        for runner in invalid {
            let error = router.register(Arc::new(runner)).unwrap_err();
            assert!(matches!(error, RouterError::InvalidCapability { .. }));
        }

        assert!(router.capabilities().runners().is_empty());
    }

    #[test]
    fn capabilities_are_an_owned_registration_snapshot() {
        let mut labels = Labels::new();
        labels.insert("zone", "eu");
        let mut router = RunnerRouter::new();
        router
            .register_with_labels(Arc::new(SubprocessRunnerDummy), labels)
            .unwrap();

        let snapshot = router.capabilities();
        router
            .register(Arc::new(DeclaredRunner {
                name: "image-resize",
                workload_types: vec![
                    WorkloadTypeMeta::new("tasks.example.io/v1", "ImageResize").unwrap(),
                ],
            }))
            .unwrap();

        assert_eq!(snapshot.runners().len(), 1);
        let capability = &snapshot.runners()[0];
        assert_eq!(capability.name(), "subprocess-only");
        assert_eq!(capability.labels().get("zone"), Some("eu"));
        assert_eq!(
            capability
                .workload_types()
                .iter()
                .map(|workload| (workload.api_version(), workload.kind()))
                .collect::<Vec<_>>(),
            vec![("solti.io/v1", "Subprocess")]
        );
        assert_eq!(router.capabilities().runners().len(), 2);
    }

    #[test]
    fn runner_must_use_the_allocated_run_id() {
        struct WrongNameRunner;

        impl Runner for WrongNameRunner {
            fn name(&self) -> &str {
                "wrong-name"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                        .expect("built-in workload GVK"),
                ]
            }

            fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(
                    "ignored-run-id",
                    |_ctx: TaskContext| async move { Ok::<(), TaskError>(()) },
                ))
            }
        }

        let task = mk_task(TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: Vec::new(),
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        )));
        let mut router = RunnerRouter::new();
        router.register(Arc::new(WrongNameRunner)).unwrap();

        match router.build(&task) {
            Err(RouterError::RunIdMismatch { .. }) => {}
            Err(error) => panic!("expected RunIdMismatch, got {error:?}"),
            Ok(_) => panic!("expected RunIdMismatch, got Ok(..)"),
        }
    }
}
