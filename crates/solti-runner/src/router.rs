//! # Runner router
//!
//! [`RunnerRouter`] owns runner registration and selection.
//! [`RunnerCatalog`] is a cloneable, immutable snapshot for runner composition.
//!
//! ## Flow
//!
//! ```text
//! register
//!    ├── validate name, labels, and workload GVKs
//!    └── store immutable capability snapshot
//!
//! Task
//!    ├── exact workload GVK
//!    ├── optional runnerSelector
//!    └── registration order
//!            ▼
//!          Runner
//! ```
//!
//! The first matching registration wins.
//! [`TaskWorkload::Embedded`](solti_model::TaskWorkload::Embedded) is not routed.
use std::sync::Arc;

use solti_model::{AgentCapabilities, Labels, RunnerCapability, Task, TaskWorkload};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::error::RouterError;
use crate::runner::Runner;
use crate::{context::BuildContext, id::make_run_id, output::OutputPublisherHandle};

/// Single runner entry with optional static labels used for routing.
#[derive(Clone)]
struct RunnerEntry {
    /// Concrete runner implementation.
    runner: Arc<dyn Runner>,
    /// Immutable routing and discovery metadata captured at registration.
    capability: RunnerCapability,
}

/// Cloneable, immutable snapshot of runner registrations.
///
/// A catalog preserves the runners, capability labels, and registration order captured by [`RunnerRouter::catalog`].
/// Later router registrations do not change an existing catalog.
///
/// Composing runners use [`build`](Self::build) to route an inner task with an explicitly provided [`BuildContext`].
/// Selection, [`RunId`](crate::RunId) allocation, and returned task-name validation are identical to [`RunnerRouter::build`].
#[derive(Clone)]
pub struct RunnerCatalog {
    runners: Arc<[RunnerEntry]>,
}

impl RunnerCatalog {
    /// Selects a snapshotted runner and builds a [`TaskRef`].
    ///
    /// The provided context is passed to the selected runner.
    /// The catalog allocates one [`RunId`](crate::RunId), and the returned task name must equal [`RunId::name`](crate::RunId::name).
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when selection or task construction fails.
    /// Returns [`RouterError::RunIdMismatch`] when the task name is incorrect.
    #[instrument(
        level = "debug",
        skip(self, task, ctx),
        fields(
            task = %task.name(),
            api_version = task.spec().workload().api_version(),
            kind = task.spec().workload().kind()
        )
    )]
    pub fn build(&self, task: &Task, ctx: &BuildContext) -> Result<TaskRef, RouterError> {
        build_from_entries(&self.runners, task, ctx)
    }
}

/// Router that selects a [`Runner`] for a [`Task`].
///
/// Registration creates a [`RunnerCapability`] snapshot.
/// Routing and [`capabilities`](Self::capabilities) use that snapshot.
///
/// ## Rules
///
/// - Runner names are unique.
/// - Workload GVK matching is exact.
/// - Labels are checked only when a task has `runnerSelector`.
/// - Registration order defines routing priority.
/// - Embedded workloads are not routed.
/// - The router allocates the [`RunId`](crate::RunId).
#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<RunnerEntry>,
    ctx: BuildContext,
}

impl RunnerRouter {
    /// Creates an empty router with the default build context.
    #[inline]
    pub fn new() -> Self {
        Self {
            runners: Vec::new(),
            ctx: BuildContext::default(),
        }
    }

    /// Sets the build context passed to selected runners.
    #[inline]
    pub fn with_context(mut self, ctx: BuildContext) -> Self {
        self.ctx = ctx;
        self
    }

    /// Replaces the output publisher in the current build context.
    ///
    /// Environment and metrics remain unchanged.
    #[inline]
    pub fn with_output_publisher(mut self, publisher: OutputPublisherHandle) -> Self {
        self.ctx = self.ctx.with_output_publisher(publisher);
        self
    }

    /// Captures the current runner registrations for composition.
    ///
    /// The returned [`RunnerCatalog`] is immutable and cheap to clone.
    /// It preserves capability labels and routing priority at the time of this call.
    /// Runners registered afterward are visible to this router but not to the catalog.
    pub fn catalog(&self) -> RunnerCatalog {
        RunnerCatalog {
            runners: self.runners.clone().into(),
        }
    }

    /// Registers a runner without labels.
    ///
    /// The runner name and workload GVKs are read once.
    /// Registration stores them as a validated capability snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] when the name is already registered.
    /// Returns [`RouterError::InvalidCapability`] when the declaration is invalid.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) -> Result<(), RouterError> {
        self.register_with_labels(runner, Labels::default())
    }

    /// Registers a runner with static labels.
    ///
    /// Labels participate only in `runnerSelector` matching.
    /// The name, labels, and workload GVKs are captured at registration.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] when the name is already registered.
    /// Returns [`RouterError::InvalidLabels`] when labels violate model rules.
    /// Returns [`RouterError::InvalidCapability`] when the declaration is invalid.
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

    /// Returns an owned snapshot of registered runner capabilities.
    ///
    /// Runners remain in routing priority order.
    /// Workload GVKs use the canonical order produced during registration.
    pub fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::new(
            self.runners
                .iter()
                .map(|entry| entry.capability.clone())
                .collect(),
        )
        .expect("RunnerRouter registration preserves unique runner names")
    }

    /// Selects the first runner that matches the task.
    ///
    /// Selection first matches the exact workload GVK.
    /// It then applies `runnerSelector` to registered labels.
    /// Registration order breaks ties.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::EmbeddedWorkload`] for embedded workloads.
    /// Returns [`RouterError::NoRunner`] when no registration matches.
    pub fn pick(&self, task: &Task) -> Result<&dyn Runner, RouterError> {
        Ok(pick_entry(&self.runners, task)?.runner.as_ref())
    }

    /// Builds a [`TaskRef`] with the selected runner.
    ///
    /// The router allocates one [`RunId`](crate::RunId).
    /// It passes the id and build context to the runner.
    /// The returned task name must equal [`RunId::name`](crate::RunId::name).
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when selection or task construction fails.
    /// Returns [`RouterError::RunIdMismatch`] when the task name is incorrect.
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
        build_from_entries(&self.runners, task, &self.ctx)
    }
}

fn pick_entry<'a>(runners: &'a [RunnerEntry], task: &Task) -> Result<&'a RunnerEntry, RouterError> {
    let selector = task.spec().runner_selector();
    let workload = task.spec().workload();
    if matches!(workload, TaskWorkload::Embedded(_)) {
        return Err(RouterError::EmbeddedWorkload);
    }
    let workload_type = workload.type_meta();

    let mut matching = runners.iter().filter(|entry| {
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

fn build_from_entries(
    runners: &[RunnerEntry],
    task: &Task,
    ctx: &BuildContext,
) -> Result<TaskRef, RouterError> {
    trace!(task = ?task, "router received task");

    let entry = pick_entry(runners, task)?;
    let runner = entry.runner.as_ref();
    let runner_name = entry.capability.name().to_owned();
    let run_id = make_run_id(&runner_name, task.slot().as_str());
    let task_ref = runner
        .build_task(task, &run_id, ctx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OutputPublisher, OutputSink, RunId, RunnerEnv, RunnerError};

    use solti_model::{
        AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, LabelSelector, Labels,
        SubprocessMode, SubprocessSpec, TaskEnv, TaskId, TaskSpec, WasmSpec, WorkloadTypeMeta,
    };
    use std::path::PathBuf;
    use taskvisor::{TaskContext, TaskError, TaskFn};

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

    fn workload_type(api_version: &str, kind: &str) -> WorkloadTypeMeta {
        WorkloadTypeMeta::new(api_version, kind).expect("valid test workload GVK")
    }

    fn subprocess_runner(name: &'static str) -> DeclaredRunner {
        DeclaredRunner {
            name,
            workload_types: vec![workload_type(
                solti_model::WORKLOAD_API_VERSION,
                "Subprocess",
            )],
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

    fn subprocess_task() -> Task {
        mk_task(TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: vec!["hello".into()],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        )))
    }

    #[test]
    fn embedded_workloads_are_not_routed_by_pick_or_build() {
        let router = RunnerRouter::new();
        let catalog = router.catalog();
        let task = mk_task(TaskWorkload::Embedded(
            EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
        ));

        assert!(matches!(
            router.pick(&task),
            Err(RouterError::EmbeddedWorkload)
        ));
        assert!(matches!(
            router.build(&task),
            Err(RouterError::EmbeddedWorkload)
        ));
        assert!(matches!(
            catalog.build(&task, &BuildContext::default()),
            Err(RouterError::EmbeddedWorkload)
        ));
    }

    #[test]
    fn build_fails_when_no_runner_supports_kind() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();

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
    fn build_passes_context_and_allocated_run_id() {
        struct EnabledOutput;

        impl OutputPublisher for EnabledOutput {
            fn sink_for(
                &self,
                _task_name: &TaskId,
                generation: u64,
                attempt: u32,
            ) -> Option<OutputSink> {
                Some(OutputSink::new(generation, attempt, |_| {}))
            }
        }

        struct ContextRunner;

        impl Runner for ContextRunner {
            fn name(&self) -> &str {
                "context"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![workload_type(
                    solti_model::WORKLOAD_API_VERSION,
                    "Subprocess",
                )]
            }

            fn build_task(
                &self,
                _task: &Task,
                run_id: &RunId,
                ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                if ctx.env().get("AGENT_ROOT") != Some("/opt/agent") {
                    return Err(RunnerError::Internal("missing runner environment".into()));
                }
                if ctx
                    .output_publisher()
                    .sink_for(&TaskId::new("test-task").unwrap(), 4, 2)
                    .is_none()
                {
                    return Err(RunnerError::Internal("missing output publisher".into()));
                }
                Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
            }
        }

        let mut env = RunnerEnv::new();
        env.push("AGENT_ROOT", "/opt/agent");
        let mut router = RunnerRouter::new()
            .with_context(BuildContext::default().with_env(env))
            .with_output_publisher(Arc::new(EnabledOutput));
        router.register(Arc::new(ContextRunner)).unwrap();

        let task = router.build(&subprocess_task()).unwrap();
        assert!(task.name().starts_with("context-test-slot-"));
    }

    #[test]
    fn catalog_is_a_cloneable_snapshot_with_explicit_context_and_exact_routing() {
        struct CatalogOutput;

        impl OutputPublisher for CatalogOutput {
            fn sink_for(
                &self,
                _task_name: &TaskId,
                generation: u64,
                attempt: u32,
            ) -> Option<OutputSink> {
                Some(OutputSink::new(generation, attempt, |_| {}))
            }
        }

        struct CatalogContextRunner {
            name: &'static str,
        }

        impl Runner for CatalogContextRunner {
            fn name(&self) -> &str {
                self.name
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![workload_type(
                    solti_model::WORKLOAD_API_VERSION,
                    "Subprocess",
                )]
            }

            fn build_task(
                &self,
                _task: &Task,
                run_id: &RunId,
                ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                if ctx.env().get("CATALOG_CONTEXT") != Some("provided") {
                    return Err(RunnerError::Internal(
                        "catalog did not pass the explicit context".into(),
                    ));
                }
                if ctx
                    .output_publisher()
                    .sink_for(&TaskId::new("test-task").unwrap(), 2, 1)
                    .is_none()
                {
                    return Err(RunnerError::Internal(
                        "catalog did not pass the explicit output publisher".into(),
                    ));
                }
                Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
            }
        }

        let mut router = RunnerRouter::new();
        for (name, zone) in [
            ("catalog-eu", "eu"),
            ("catalog-us-first", "us"),
            ("catalog-us-second", "us"),
        ] {
            let mut labels = Labels::new();
            labels.insert("zone", zone);
            router
                .register_with_labels(Arc::new(CatalogContextRunner { name }), labels)
                .unwrap();
        }

        let catalog = router.catalog();
        router
            .register(Arc::new(DeclaredRunner {
                name: "registered-later",
                workload_types: vec![workload_type("tasks.example.io/v1", "ImageResize")],
            }))
            .unwrap();

        let mut match_labels = Labels::new();
        match_labels.insert("zone", "us");
        let (_, metadata, spec, status) = subprocess_task().into_parts();
        let selected_task = Task::from_parts(
            solti_model::TypeMeta::task(),
            metadata,
            spec.with_runner_selector(LabelSelector::from_labels(match_labels)),
            status,
        )
        .unwrap();
        let mut env = RunnerEnv::new();
        env.push("CATALOG_CONTEXT", "provided");
        let explicit_ctx = BuildContext::default()
            .with_env(env)
            .with_output_publisher(Arc::new(CatalogOutput));

        let cloned_catalog = catalog.clone();
        drop(catalog);
        let built = cloned_catalog.build(&selected_task, &explicit_ctx).unwrap();
        assert!(built.name().starts_with("catalog-us-first-test-slot-"));

        let later_task = mk_task(TaskWorkload::Extension(
            solti_model::ExtensionWorkload::new(
                "tasks.example.io/v1",
                "ImageResize",
                serde_json::json!({ "width": 1280 }),
            )
            .unwrap(),
        ));
        assert!(matches!(
            cloned_catalog.build(&later_task, &explicit_ctx),
            Err(RouterError::NoRunner { .. })
        ));
        assert!(router.build(&later_task).is_ok());
    }

    #[test]
    fn pick_applies_gvk_selector_and_registration_order() {
        let mut router = RunnerRouter::new();
        for (name, zone) in [("r1", "eu"), ("r2", "us"), ("r3", "us")] {
            let mut labels = Labels::new();
            labels.insert("zone", zone);
            router
                .register_with_labels(Arc::new(subprocess_runner(name)), labels)
                .unwrap();
        }

        let mut match_labels = Labels::new();
        match_labels.insert("zone", "us");
        let (_, metadata, spec, status) = subprocess_task().into_parts();
        let task = Task::from_parts(
            solti_model::TypeMeta::task(),
            metadata,
            spec.with_runner_selector(LabelSelector::from_labels(match_labels)),
            status,
        )
        .unwrap();

        let picked = router.pick(&task).expect("runner should be picked");
        assert_eq!(picked.name(), "r2");
    }

    #[test]
    fn application_defined_gvk_routes_through_registered_runner() {
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
        router
            .register(Arc::new(DeclaredRunner {
                name: "image-resize",
                workload_types: vec![workload_type("tasks.example.io/v1", "ImageResize")],
            }))
            .unwrap();

        let built = router.build(&task).unwrap();
        assert!(built.name().starts_with("image-resize-test-slot-"));
    }

    #[test]
    fn duplicate_runner_name_is_rejected() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();

        let error = router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap_err();

        assert!(matches!(error, RouterError::DuplicateRunner { .. }));
    }

    #[test]
    fn invalid_runner_labels_are_rejected_without_registration() {
        let mut labels = Labels::new();
        labels.insert("invalid label key", "value");
        let mut router = RunnerRouter::new();

        let error = router
            .register_with_labels(Arc::new(subprocess_runner("subprocess-only")), labels)
            .unwrap_err();

        assert!(matches!(error, RouterError::InvalidLabels { .. }));
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();
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
                    workload_type(solti_model::WORKLOAD_API_VERSION, "Subprocess"),
                    workload_type(solti_model::WORKLOAD_API_VERSION, "Subprocess"),
                ],
            },
            DeclaredRunner {
                name: "embedded-workload",
                workload_types: vec![workload_type(solti_model::WORKLOAD_API_VERSION, "Embedded")],
            },
            DeclaredRunner {
                name: "invalid/name",
                workload_types: vec![workload_type(
                    solti_model::WORKLOAD_API_VERSION,
                    "Subprocess",
                )],
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
            .register_with_labels(Arc::new(subprocess_runner("subprocess-only")), labels)
            .unwrap();

        let snapshot = router.capabilities();
        router
            .register(Arc::new(DeclaredRunner {
                name: "image-resize",
                workload_types: vec![workload_type("tasks.example.io/v1", "ImageResize")],
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
    fn build_keeps_runner_name_and_source_error() {
        struct FailingRunner;

        impl Runner for FailingRunner {
            fn name(&self) -> &str {
                "failing"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![workload_type(
                    solti_model::WORKLOAD_API_VERSION,
                    "Subprocess",
                )]
            }

            fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                _ctx: &BuildContext,
            ) -> Result<TaskRef, RunnerError> {
                Err(RunnerError::InvalidSpec("missing command".into()))
            }
        }

        let mut router = RunnerRouter::new();
        router.register(Arc::new(FailingRunner)).unwrap();

        let error = match router.build(&subprocess_task()) {
            Err(error) => error,
            Ok(_) => panic!("expected runner build failure"),
        };
        assert!(matches!(
            error,
            RouterError::Build {
                runner,
                source: RunnerError::InvalidSpec(message),
            } if runner == "failing" && message == "missing command"
        ));
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

        let mut router = RunnerRouter::new();
        router.register(Arc::new(WrongNameRunner)).unwrap();
        let catalog = router.catalog();

        match router.build(&subprocess_task()) {
            Err(RouterError::RunIdMismatch { .. }) => {}
            Err(error) => panic!("expected RunIdMismatch, got {error:?}"),
            Ok(_) => panic!("expected RunIdMismatch, got Ok(..)"),
        }
        match catalog.build(&subprocess_task(), &BuildContext::default()) {
            Err(RouterError::RunIdMismatch { .. }) => {}
            Err(error) => panic!("expected RunIdMismatch, got {error:?}"),
            Ok(_) => panic!("expected RunIdMismatch, got Ok(..)"),
        }
    }
}
