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
//! [`TaskWorkload::Embedded`] is not routed.
use std::sync::Arc;

use solti_model::{AgentCapabilities, Labels, RunnerCapability, Task, TaskWorkload};
use taskvisor::TaskRef;
use tracing::{debug, instrument, trace};

use crate::admission::{BuildScope, EnterBuildError, RunnerBuildAdmission};
use crate::cancellation::BuildCancellation;
use crate::error::RouterError;
use crate::runner::Runner;
use crate::{
    context::BuildContext,
    id::{RunId, make_run_id},
    output::OutputPublisherHandle,
};

/// Single runner entry with optional static labels used for routing.
#[derive(Clone)]
struct RunnerEntry {
    /// Concrete runner implementation.
    runner: Arc<dyn Runner>,
    /// Immutable routing and discovery metadata captured at registration.
    capability: RunnerCapability,
}

/// One runner-built task paired with the run identity allocated by the router.
///
/// Taskvisor task objects do not carry registration identity.
/// Use [`name`](Self::name) when constructing the surrounding `taskvisor::TaskSpec`, and use
/// [`into_task`](Self::into_task) when only the executable task is needed.
#[must_use]
pub struct BuiltTask {
    run_id: RunId,
    task: TaskRef,
}

impl BuiltTask {
    fn new(run_id: RunId, task: TaskRef) -> Self {
        Self { run_id, task }
    }

    /// Returns the router-allocated run identity.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Returns the name allocated for this run.
    ///
    /// This is the name to pass to `taskvisor::TaskSpec` when the built task is submitted for supervision.
    pub fn name(&self) -> &str {
        self.run_id.name()
    }

    /// Returns the executable Taskvisor task.
    pub fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Consumes the build result and returns only its executable task.
    pub fn into_task(self) -> TaskRef {
        self.task
    }

    /// Consumes the build result and returns its run identity and executable task.
    pub fn into_parts(self) -> (RunId, TaskRef) {
        (self.run_id, self.task)
    }
}

impl std::fmt::Debug for BuiltTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltTask")
            .field("run_id", &self.run_id)
            .field("task", &"<dyn Task>")
            .finish()
    }
}

/// Cloneable, immutable snapshot of runner registrations.
///
/// A catalog preserves the runners, capability labels, and registration order captured by [`RunnerRouter::catalog`].
/// Later router registrations do not change an existing catalog.
///
/// Composing runners use [`build`](Self::build) to route an inner task with an explicitly provided [`BuildContext`].
/// Selection and [`RunId`] allocation are identical to [`RunnerRouter::build`].
#[derive(Clone)]
pub struct RunnerCatalog {
    runners: Arc<[RunnerEntry]>,
}

impl RunnerCatalog {
    /// Selects a snapshotted runner and builds a [`BuiltTask`].
    ///
    /// This direct build is unmanaged and does not apply core admission limits.
    /// The provided context is passed to the selected runner.
    /// The catalog allocates one [`RunId`] and returns it with the executable task.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when selection or task construction fails.
    #[instrument(
        level = "debug",
        skip(self, task, ctx),
        fields(
            event = "runner.build",
            task_name = %task.name(),
            generation = task.metadata().generation(),
            workload_api_version = task.spec().workload().api_version(),
            workload_kind = task.spec().workload().kind()
        )
    )]
    pub async fn build(&self, task: &Task, ctx: &BuildContext) -> Result<BuiltTask, RouterError> {
        self.build_with_cancellation(task, ctx, BuildCancellation::new())
            .await
    }

    /// Selects a snapshotted runner and builds with an external cancellation signal.
    ///
    /// The caller retains the matching [`BuildCancellationHandle`](crate::BuildCancellationHandle).
    /// The build future owns all runner work.
    /// Cancellation is cooperative for child operations and dropping the future cancels the build scope.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::build`].
    pub async fn build_with_cancellation(
        &self,
        task: &Task,
        ctx: &BuildContext,
        cancellation: BuildCancellation,
    ) -> Result<BuiltTask, RouterError> {
        build_unmanaged_from_entries(&self.runners, task, ctx, &cancellation).await
    }

    /// Selects and builds a nested runner within an inherited admission scope.
    ///
    /// The nested build reuses the outer global permit and acquires the selected runner's per-runner permit.
    /// Composing runners must use this method instead of [`Self::build`] so managed core limits include every nested runner.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::RecursiveBuild`] when the selected runner already exists in the active build path.
    /// Returns [`RouterError::AdmissionCycle`] when the nested wait would deadlock with other active root builds.
    /// Returns [`RouterError::BuildCancelled`] when cancellation wins while the build waits for a per-runner permit.
    /// Otherwise, returns the same errors as [`Self::build`].
    pub async fn build_scoped_with_cancellation(
        &self,
        task: &Task,
        ctx: &BuildContext,
        cancellation: &BuildCancellation,
        scope: &mut BuildScope,
    ) -> Result<BuiltTask, RouterError> {
        build_scoped_from_entries(&self.runners, task, ctx, cancellation, scope).await
    }
}

/// One outer runner build after managed admission succeeds.
///
/// The value owns the global and outer-runner permits until [`build`](Self::build) finishes or the value is dropped.
#[must_use]
pub struct AdmittedBuild {
    entry: RunnerEntry,
    task: Task,
    ctx: BuildContext,
    cancellation: BuildCancellation,
    scope: BuildScope,
}

impl AdmittedBuild {
    /// Builds the admitted task.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when runner construction fails.
    pub async fn build(mut self) -> Result<BuiltTask, RouterError> {
        build_entry(
            &self.entry,
            &self.task,
            &self.ctx,
            &self.cancellation,
            &mut self.scope,
        )
        .await
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
/// - The router allocates the [`RunId`].
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
    /// Returns [`RouterError::InvalidCapability`] when the declaration is invalid.
    /// Returns [`RouterError::InvalidLabels`] when labels violate model rules.
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

    /// Returns the selected runner name without building the task.
    ///
    /// Core uses this stable registration snapshot for per-runner admission.
    ///
    /// # Errors
    ///
    /// Returns the same selection errors as [`Self::pick`].
    pub fn runner_name<'a>(&'a self, task: &Task) -> Result<&'a str, RouterError> {
        Ok(pick_entry(&self.runners, task)?.capability.name())
    }

    /// Builds a [`BuiltTask`] with the selected runner.
    ///
    /// This direct build is unmanaged and does not apply core admission limits.
    /// The router allocates one [`RunId`].
    /// It passes the id and build context to the runner.
    /// The returned [`BuiltTask`] keeps that identity next to the executable task.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError`] when selection or task construction fails.
    #[instrument(
        level = "debug",
        skip(self, task),
        fields(
            event = "runner.build",
            task_name = %task.name(),
            generation = task.metadata().generation(),
            workload_api_version = task.spec().workload().api_version(),
            workload_kind = task.spec().workload().kind()
        )
    )]
    pub async fn build(&self, task: &Task) -> Result<BuiltTask, RouterError> {
        self.build_with_cancellation(task, BuildCancellation::new())
            .await
    }

    /// Builds with an external cancellation signal.
    ///
    /// The caller retains the matching [`BuildCancellationHandle`](crate::BuildCancellationHandle).
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::build`].
    pub async fn build_with_cancellation(
        &self,
        task: &Task,
        cancellation: BuildCancellation,
    ) -> Result<BuiltTask, RouterError> {
        build_unmanaged_from_entries(&self.runners, task, &self.ctx, &cancellation).await
    }

    /// Acquires managed admission for one selected outer runner build.
    ///
    /// Waiting for root admission is outside the admitted build.
    /// Nested builds created by the returned value reuse its global permit.
    ///
    /// # Errors
    ///
    /// Returns the same selection errors as [`Self::pick`].
    /// Returns [`RouterError::BuildCancelled`] when cancellation wins while admission is pending.
    pub async fn admit(
        &self,
        task: &Task,
        admission: &RunnerBuildAdmission,
        cancellation: BuildCancellation,
    ) -> Result<AdmittedBuild, RouterError> {
        let entry = pick_entry(&self.runners, task)?.clone();
        let runner_name = entry.capability.name();
        let scope = admission
            .enter_root(runner_name, &cancellation)
            .await
            .map_err(|error| map_enter_error(runner_name, error))?;
        Ok(AdmittedBuild {
            entry,
            task: task.clone(),
            ctx: self.ctx.clone(),
            cancellation,
            scope,
        })
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
            event = "runner.multiple_matches",
            task_name = %task.name(),
            generation = task.metadata().generation(),
            slot = %task.slot(),
            workload_api_version = workload.api_version(),
            workload_kind = workload.kind(),
            runner = first.capability.name(),
            "multiple runners matched; using first registered"
        );
    }
    Ok(first)
}

async fn build_unmanaged_from_entries(
    runners: &[RunnerEntry],
    task: &Task,
    ctx: &BuildContext,
    cancellation: &BuildCancellation,
) -> Result<BuiltTask, RouterError> {
    trace!(
        event = "runner.route",
        task_name = %task.name(),
        generation = task.metadata().generation(),
        slot = %task.slot(),
        workload_api_version = task.spec().workload().api_version(),
        workload_kind = task.spec().workload().kind(),
        "router received task"
    );
    let entry = pick_entry(runners, task)?;
    let mut scope = BuildScope::unmanaged(entry.capability.name());
    build_entry(entry, task, ctx, cancellation, &mut scope).await
}

async fn build_scoped_from_entries(
    runners: &[RunnerEntry],
    task: &Task,
    ctx: &BuildContext,
    cancellation: &BuildCancellation,
    scope: &mut BuildScope,
) -> Result<BuiltTask, RouterError> {
    trace!(
        event = "runner.route",
        task_name = %task.name(),
        generation = task.metadata().generation(),
        slot = %task.slot(),
        workload_api_version = task.spec().workload().api_version(),
        workload_kind = task.spec().workload().kind(),
        "router received nested task"
    );

    let entry = pick_entry(runners, task)?;
    let runner_name = entry.capability.name();
    let mut child_scope = scope
        .enter_child(runner_name, cancellation)
        .await
        .map_err(|error| map_enter_error(runner_name, error))?;
    build_entry(entry, task, ctx, cancellation, &mut child_scope).await
}

async fn build_entry(
    entry: &RunnerEntry,
    task: &Task,
    ctx: &BuildContext,
    cancellation: &BuildCancellation,
    scope: &mut BuildScope,
) -> Result<BuiltTask, RouterError> {
    let runner = entry.runner.as_ref();
    let runner_name = entry.capability.name().to_owned();
    let run_id = make_run_id(&runner_name, task.slot().as_str());
    let task_ref = runner
        .build_task(task, &run_id, ctx, cancellation, scope)
        .await
        .map_err(|source| RouterError::Build {
            runner: runner_name.clone(),
            source,
        })?;
    debug!(
        event = "runner.built",
        task_name = %task.name(),
        generation = task.metadata().generation(),
        runner = runner_name,
        run_id = run_id.name(),
        "runner built task"
    );
    Ok(BuiltTask::new(run_id, task_ref))
}

fn map_enter_error(runner: &str, error: EnterBuildError) -> RouterError {
    match error {
        EnterBuildError::AdmissionCycle => RouterError::AdmissionCycle {
            runner: runner.to_owned(),
        },
        EnterBuildError::Cancelled => RouterError::BuildCancelled {
            runner: runner.to_owned(),
        },
        EnterBuildError::Recursive => RouterError::RecursiveBuild {
            runner: runner.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OutputPublisher, OutputSink, RunId, RunnerEnv, RunnerError};

    use solti_model::{
        AdmissionPolicy, BackoffPolicy, EmbeddedSpec, Flag, JitterPolicy, LabelSelector, Labels,
        SubprocessMode, SubprocessSpec, TaskEnv, TaskId, TaskSpec, WasmSpec, WorkloadTypeMeta,
    };
    use std::{fmt, path::PathBuf, sync::Mutex};
    use taskvisor::{TaskContext, TaskError, TaskFn};
    use tracing::{
        Event, Metadata, Subscriber,
        field::{Field, Visit},
        instrument::WithSubscriber as _,
        span::{Attributes, Id, Record},
    };

    #[derive(Default)]
    struct TraceCapture {
        fields: Mutex<Vec<String>>,
    }

    struct CaptureSubscriber(Arc<TraceCapture>);

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            event.record(&mut CaptureVisitor(&self.0));
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    struct CaptureVisitor<'a>(&'a TraceCapture);

    impl Visit for CaptureVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0
                .fields
                .lock()
                .unwrap()
                .push(format!("{}={value:?}", field.name()));
        }
    }

    struct DeclaredRunner {
        name: &'static str,
        workload_types: Vec<WorkloadTypeMeta>,
    }

    #[crate::async_trait]
    impl Runner for DeclaredRunner {
        fn name(&self) -> &str {
            self.name
        }

        fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
            self.workload_types.clone()
        }

        async fn build_task(
            &self,
            _task: &Task,
            _run_id: &RunId,
            _ctx: &BuildContext,
            _cancellation: &BuildCancellation,
            _scope: &mut BuildScope,
        ) -> Result<TaskRef, RunnerError> {
            Ok(TaskFn::arc(|_ctx: TaskContext| async move {
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

    #[tokio::test]
    async fn embedded_workloads_are_not_routed_by_pick_or_build() {
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
            router.build(&task).await,
            Err(RouterError::EmbeddedWorkload)
        ));
        assert!(matches!(
            catalog.build(&task, &BuildContext::default()).await,
            Err(RouterError::EmbeddedWorkload)
        ));
    }

    #[tokio::test]
    async fn scoped_catalog_rejects_same_runner_reentry_with_typed_error() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();
        let catalog = router.catalog();
        let task = subprocess_task();
        let cancellation = BuildCancellation::new();
        let admission = RunnerBuildAdmission::new(1, 1).unwrap();
        let mut scope = admission
            .enter_root("subprocess-only", &cancellation)
            .await
            .unwrap();

        let result = catalog
            .build_scoped_with_cancellation(
                &task,
                &BuildContext::default(),
                &cancellation,
                &mut scope,
            )
            .await;

        assert!(matches!(
            result,
            Err(RouterError::RecursiveBuild { runner }) if runner == "subprocess-only"
        ));
    }

    #[tokio::test]
    async fn scoped_catalog_maps_cancelled_admission_to_typed_error() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();
        let catalog = router.catalog();
        let task = subprocess_task();
        let root_cancellation = BuildCancellation::new();
        let admission = RunnerBuildAdmission::new(1, 1).unwrap();
        let mut scope = admission
            .enter_root("outer", &root_cancellation)
            .await
            .unwrap();
        let (cancel_handle, cancellation) = BuildCancellation::pair();
        cancel_handle.cancel();

        let result = catalog
            .build_scoped_with_cancellation(
                &task,
                &BuildContext::default(),
                &cancellation,
                &mut scope,
            )
            .await;

        assert!(matches!(
            result,
            Err(RouterError::BuildCancelled { runner }) if runner == "subprocess-only"
        ));
    }

    #[test]
    fn admission_cycle_maps_to_typed_router_error() {
        assert!(matches!(
            map_enter_error("nested", EnterBuildError::AdmissionCycle),
            RouterError::AdmissionCycle { runner } if runner == "nested"
        ));
    }

    #[tokio::test]
    async fn build_fails_when_no_runner_supports_kind() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();

        let task = mk_task(TaskWorkload::Wasm(WasmSpec::new(
            PathBuf::from("mod.wasm"),
            Vec::new(),
            TaskEnv::default(),
        )));

        let res = router.build(&task).await;

        match res {
            Err(RouterError::NoRunner { api_version, kind }) => {
                assert_eq!(api_version, "solti.io/v1");
                assert_eq!(kind, "Wasm");
            }
            Ok(_) => panic!("expected RouterError::NoRunner for wasm, got Ok(..)"),
            Err(e) => panic!("expected RouterError::NoRunner for wasm, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn router_trace_does_not_record_the_task_payload() {
        const SECRET: &str = "must-not-appear-in-router-trace";

        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(subprocess_runner("subprocess-only")))
            .unwrap();
        let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: vec![SECRET.into()],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        ));
        let spec = TaskSpec::builder("trace-test-slot", workload, 10_000_u64)
            .build()
            .unwrap();
        let task = Task::new("trace-test-task", spec).unwrap();
        let capture = Arc::new(TraceCapture::default());
        let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
        let _interest_guard = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
        tracing::dispatcher::with_default(&dispatch, tracing::callsite::rebuild_interest_cache);

        let _built = router.build(&task).with_subscriber(dispatch).await.unwrap();

        let fields = capture.fields.lock().unwrap().join(" ");
        assert!(fields.contains("task_name=trace-test-task"));
        assert!(fields.contains("slot=trace-test-slot"));
        assert!(!fields.contains(SECRET));
    }

    #[tokio::test]
    async fn build_passes_context_and_allocated_run_id() {
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

        #[crate::async_trait]
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

            async fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                ctx: &BuildContext,
                _cancellation: &BuildCancellation,
                _scope: &mut BuildScope,
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
                Ok(TaskFn::arc(|_ctx: TaskContext| async move {
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

        let task = router.build(&subprocess_task()).await.unwrap();
        assert!(task.name().starts_with("context-test-slot-"));
    }

    #[tokio::test]
    async fn catalog_is_a_cloneable_snapshot_with_explicit_context_and_exact_routing() {
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

        #[crate::async_trait]
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

            async fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                ctx: &BuildContext,
                _cancellation: &BuildCancellation,
                _scope: &mut BuildScope,
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
                Ok(TaskFn::arc(|_ctx: TaskContext| async move {
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
        let built = cloned_catalog
            .build(&selected_task, &explicit_ctx)
            .await
            .unwrap();
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
            cloned_catalog.build(&later_task, &explicit_ctx).await,
            Err(RouterError::NoRunner { .. })
        ));
        assert!(router.build(&later_task).await.is_ok());
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

    #[tokio::test]
    async fn application_defined_gvk_routes_through_registered_runner() {
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

        let built = router.build(&task).await.unwrap();
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

    #[tokio::test]
    async fn build_keeps_runner_name_and_source_error() {
        struct FailingRunner;

        #[crate::async_trait]
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

            async fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                _ctx: &BuildContext,
                _cancellation: &BuildCancellation,
                _scope: &mut BuildScope,
            ) -> Result<TaskRef, RunnerError> {
                Err(RunnerError::InvalidSpec("missing command".into()))
            }
        }

        let mut router = RunnerRouter::new();
        router.register(Arc::new(FailingRunner)).unwrap();

        let error = match router.build(&subprocess_task()).await {
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

    #[tokio::test]
    async fn build_result_keeps_the_allocated_run_id_beside_the_task() {
        struct IdentityAgnosticRunner;

        #[crate::async_trait]
        impl Runner for IdentityAgnosticRunner {
            fn name(&self) -> &str {
                "identity-agnostic"
            }

            fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
                vec![
                    WorkloadTypeMeta::new(solti_model::WORKLOAD_API_VERSION, "Subprocess")
                        .expect("built-in workload GVK"),
                ]
            }

            async fn build_task(
                &self,
                _task: &Task,
                _run_id: &RunId,
                _ctx: &BuildContext,
                _cancellation: &BuildCancellation,
                _scope: &mut BuildScope,
            ) -> Result<TaskRef, RunnerError> {
                Ok(TaskFn::arc(|_ctx: TaskContext| async move {
                    Ok::<(), TaskError>(())
                }))
            }
        }

        let mut router = RunnerRouter::new();
        router.register(Arc::new(IdentityAgnosticRunner)).unwrap();
        let catalog = router.catalog();

        let built = router.build(&subprocess_task()).await.unwrap();
        assert!(built.name().starts_with("identity-agnostic-test-slot-"));
        assert_eq!(built.run_id().name(), built.name());
        let name = built.name().to_owned();
        let task = Arc::clone(built.task());
        let (run_id, into_task) = built.into_parts();
        assert_eq!(run_id.name(), name);
        assert!(Arc::ptr_eq(&task, &into_task));

        let built = catalog
            .build(&subprocess_task(), &BuildContext::default())
            .await
            .unwrap();
        assert!(built.name().starts_with("identity-agnostic-test-slot-"));
        let task = Arc::clone(built.task());
        assert!(Arc::ptr_eq(&task, &built.into_task()));
    }
}
