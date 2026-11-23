//! # Runner router
//!
//! [`RunnerRouter`] owns runner registration and selection.
//! [`RunnerCatalog`] is a cloneable, immutable snapshot for runner composition.
//!
//! ## Flow
//!
//! ```text
//! register
//!    ├── validate name, labels, and workload declarations
//!    ├── check the agent-wide caps on runners and schemas
//!    └── store immutable capability snapshot and body checks
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
//! Validation picks the runner the same way and runs its declared body check,
//! so the runner that checks a body is the runner that builds the task.
//! [`TaskWorkload::Embedded`] is not routed.
use std::collections::HashMap;
use std::sync::Arc;

use solti_model::{
    AgentCapabilities, LabelSelector, Labels, RunnerCapability, Task, TaskSpec, TaskWorkload,
    Workload, WorkloadDecl, WorkloadTypeMeta,
};
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
    /// Declarations read once at registration, keyed by their GVK.
    decls: Arc<HashMap<WorkloadTypeMeta, WorkloadDecl>>,
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

    /// Checks a task's workload with the runner that would build it.
    ///
    /// The runner is picked exactly as [`Self::build`] picks it.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::EmbeddedWorkload`] for embedded workloads.
    /// Returns [`RouterError::NoRunner`] when no registration matches.
    /// Returns [`RouterError::InvalidWorkload`] when the picked runner's declaration refuses the body.
    pub fn validate(&self, spec: &TaskSpec) -> Result<(), RouterError> {
        validate_spec_with_entries(&self.runners, spec)
    }

    /// Checks one routed workload with the first runner that matches its GVK and `selector`.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRunner`] when no registration matches.
    /// Returns [`RouterError::InvalidWorkload`] when the picked runner's declaration refuses the body.
    pub fn validate_workload(
        &self,
        workload: &Workload,
        selector: Option<&LabelSelector>,
    ) -> Result<(), RouterError> {
        validate_workload_with_entries(&self.runners, workload, selector)
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
    /// The runner name and workload declarations are read once.
    /// Registration stores them as a validated capability snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] when the name is already registered.
    /// Returns [`RouterError::InvalidWorkloadType`] when a declaration is unusable.
    /// Returns [`RouterError::InvalidCapability`] when the capability is invalid or an agent-wide cap would be exceeded.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) -> Result<(), RouterError> {
        self.register_with_labels(runner, Labels::default())
    }

    /// Registers a runner with static labels.
    ///
    /// Labels participate only in `runnerSelector` matching.
    /// The name, labels, and workload declarations are captured at registration.
    ///
    /// The agent-wide caps are checked here: at most
    /// [`MAX_AGENT_RUNNERS`](solti_model::MAX_AGENT_RUNNERS) runners and
    /// [`MAX_AGENT_WORKLOAD_SCHEMAS`](solti_model::MAX_AGENT_WORKLOAD_SCHEMAS) distinct schemas.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::DuplicateRunner`] when the name is already registered.
    /// Returns [`RouterError::InvalidLabels`] when labels violate model rules.
    /// Returns [`RouterError::InvalidWorkloadType`] when a declaration is unusable.
    /// Returns [`RouterError::InvalidCapability`] when the capability is invalid or an agent-wide cap would be exceeded.
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
        let decls = runner.workload_types();
        let mut types = Vec::with_capacity(decls.len());
        for decl in &decls {
            match decl.workload_type() {
                Ok(workload_type) => types.push(workload_type.clone()),
                Err(error) => {
                    return Err(RouterError::InvalidWorkloadType {
                        runner: runner_name,
                        api_version: error.api_version().to_owned(),
                        kind: error.kind().to_owned(),
                        reason: error.reason().to_owned(),
                    });
                }
            }
        }
        let invalid_capability = |source| RouterError::InvalidCapability {
            runner: runner_name.clone(),
            source,
        };
        let capability = RunnerCapability::new(runner_name.clone(), labels, types)
            .map_err(invalid_capability)?;
        let mut capabilities: Vec<RunnerCapability> = self
            .runners
            .iter()
            .map(|entry| entry.capability.clone())
            .collect();
        capabilities.push(capability.clone());
        AgentCapabilities::new(capabilities).map_err(invalid_capability)?;
        let decls = decls
            .into_iter()
            .filter_map(|decl| {
                let meta = decl.workload_type().ok()?.meta().clone();
                Some((meta, decl))
            })
            .collect();
        self.runners.push(RunnerEntry {
            runner,
            capability,
            decls: Arc::new(decls),
        });
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
        .expect("RunnerRouter registration checks the agent-wide caps")
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

    /// Checks a task's workload with the runner that would build it.
    ///
    /// The runner is picked exactly as [`Self::build`] picks it.
    /// Admission calls this before it stores a spec.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::EmbeddedWorkload`] for embedded workloads.
    /// Returns [`RouterError::NoRunner`] when no registration matches.
    /// Returns [`RouterError::InvalidWorkload`] when the picked runner's declaration refuses the body.
    pub fn validate(&self, spec: &TaskSpec) -> Result<(), RouterError> {
        validate_spec_with_entries(&self.runners, spec)
    }

    /// Checks one routed workload with the first runner that matches its GVK and `selector`.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRunner`] when no registration matches.
    /// Returns [`RouterError::InvalidWorkload`] when the picked runner's declaration refuses the body.
    pub fn validate_workload(
        &self,
        workload: &Workload,
        selector: Option<&LabelSelector>,
    ) -> Result<(), RouterError> {
        validate_workload_with_entries(&self.runners, workload, selector)
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
    let workload = task.spec().workload();
    let TaskWorkload::Routed(routed) = workload else {
        return Err(RouterError::EmbeddedWorkload);
    };
    let (first, multiple) = select(runners, &routed.type_meta(), task.spec().runner_selector())?;
    if multiple {
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

/// Picks the first entry, in registration order, that declares `meta` and matches `selector`.
///
/// The flag tells whether another entry matched too.
fn select<'a>(
    runners: &'a [RunnerEntry],
    meta: &WorkloadTypeMeta,
    selector: Option<&LabelSelector>,
) -> Result<(&'a RunnerEntry, bool), RouterError> {
    let mut matching = runners.iter().filter(|entry| {
        entry.decls.contains_key(meta)
            && selector.is_none_or(|sel| sel.matches(entry.capability.labels()))
    });
    let first = matching.next().ok_or_else(|| RouterError::NoRunner {
        api_version: meta.api_version().to_owned(),
        kind: meta.kind().to_owned(),
    })?;
    Ok((first, matching.next().is_some()))
}

fn validate_spec_with_entries(runners: &[RunnerEntry], spec: &TaskSpec) -> Result<(), RouterError> {
    let TaskWorkload::Routed(workload) = spec.workload() else {
        return Err(RouterError::EmbeddedWorkload);
    };
    validate_workload_with_entries(runners, workload, spec.runner_selector())
}

fn validate_workload_with_entries(
    runners: &[RunnerEntry],
    workload: &Workload,
    selector: Option<&LabelSelector>,
) -> Result<(), RouterError> {
    let meta = workload.type_meta();
    let (entry, _) = select(runners, &meta, selector)?;
    let decl = entry
        .decls
        .get(&meta)
        .expect("a selected entry declares the workload GVK");
    decl.check(workload)
        .map_err(|reason| RouterError::InvalidWorkload {
            runner: entry.capability.name().to_owned(),
            api_version: meta.api_version().to_owned(),
            kind: meta.kind().to_owned(),
            reason,
        })
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

    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use solti_model::{
        AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, TaskId,
        WORKLOAD_SCHEMA_DIALECT, WorkloadBody,
    };
    use std::{fmt, sync::Mutex};
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
        workload_types: Vec<WorkloadDecl>,
    }

    #[crate::async_trait]
    impl Runner for DeclaredRunner {
        fn name(&self) -> &str {
            self.name
        }

        fn workload_types(&self) -> Vec<WorkloadDecl> {
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

    const EXEC: &str = "exec.solti.io/v1";
    const EXAMPLE: &str = "tasks.example.io/v1";

    fn schema(tag: &str) -> Value {
        json!({ "$schema": WORKLOAD_SCHEMA_DIALECT, "type": "object", "description": tag })
    }

    fn decl(api_version: &str, kind: &str) -> WorkloadDecl {
        WorkloadDecl::new(api_version, kind, &schema("any"), |_| Ok(()))
    }

    fn rejecting(api_version: &str, kind: &str, reason: &'static str) -> WorkloadDecl {
        WorkloadDecl::new(api_version, kind, &schema("any"), move |_| {
            Err(reason.to_owned())
        })
    }

    fn routed(api_version: &str, kind: &str, body: Value) -> Workload {
        Workload::new(api_version, kind, body).expect("valid test workload")
    }

    fn selector(key: &str, value: &str) -> LabelSelector {
        LabelSelector::from_labels(labels(key, value))
    }

    fn labels(key: &str, value: &str) -> Labels {
        let mut labels = Labels::new();
        labels.insert(key, value);
        labels
    }

    fn with_selector(task: Task, selector: LabelSelector) -> Task {
        let (_, metadata, spec, status) = task.into_parts();
        Task::from_parts(
            solti_model::TypeMeta::task(),
            metadata,
            spec.with_runner_selector(selector),
            status,
        )
        .unwrap()
    }

    fn subprocess_runner(name: &'static str) -> DeclaredRunner {
        DeclaredRunner {
            name,
            workload_types: vec![decl(EXEC, "Subprocess")],
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
        mk_task(TaskWorkload::Routed(routed(
            EXEC,
            "Subprocess",
            json!({ "command": "echo" }),
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

        let task = mk_task(TaskWorkload::Routed(routed(
            EXAMPLE,
            "Wasm",
            json!({ "module": "mod.wasm" }),
        )));

        let res = router.build(&task).await;

        match res {
            Err(RouterError::NoRunner { api_version, kind }) => {
                assert_eq!(api_version, EXAMPLE);
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
        let workload = TaskWorkload::Routed(routed(
            EXEC,
            "Subprocess",
            json!({ "command": "echo", "args": [SECRET] }),
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

            fn workload_types(&self) -> Vec<WorkloadDecl> {
                vec![decl(EXEC, "Subprocess")]
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

            fn workload_types(&self) -> Vec<WorkloadDecl> {
                vec![decl(EXEC, "Subprocess")]
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
                workload_types: vec![decl("tasks.example.io/v1", "ImageResize")],
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

        let later_task = mk_task(TaskWorkload::Routed(routed(
            EXAMPLE,
            "ImageResize",
            json!({ "width": 1280 }),
        )));
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
        let workload =
            TaskWorkload::Routed(routed(EXAMPLE, "ImageResize", json!({ "width": 1280 })));
        let task = mk_task(workload);
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(DeclaredRunner {
                name: "image-resize",
                workload_types: vec![decl("tasks.example.io/v1", "ImageResize")],
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
                workload_types: vec![decl(EXEC, "Subprocess"), decl(EXEC, "Subprocess")],
            },
            DeclaredRunner {
                name: "invalid/name",
                workload_types: vec![decl(EXEC, "Subprocess")],
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
                workload_types: vec![decl("tasks.example.io/v1", "ImageResize")],
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
            vec![(EXEC, "Subprocess")]
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

            fn workload_types(&self) -> Vec<WorkloadDecl> {
                vec![decl(EXEC, "Subprocess")]
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

            fn workload_types(&self) -> Vec<WorkloadDecl> {
                vec![decl(EXEC, "Subprocess")]
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

    // --- declared types, first-match routing, validation ---

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)] // only decoded and described
    struct Resize {
        width: u32,
    }

    impl WorkloadBody for Resize {
        const API_VERSION: &'static str = EXAMPLE;
        const KIND: &'static str = "Resize";
    }

    #[test]
    fn registration_snapshots_declared_types_with_fingerprints() {
        let mut router = RunnerRouter::new();
        let first = decl(EXAMPLE, "Beta");
        let second = WorkloadDecl::new(EXAMPLE, "Alpha", &schema("other"), |_| Ok(()));
        let expected_beta = first.workload_type().unwrap().fingerprint().clone();
        let expected_alpha = second.workload_type().unwrap().fingerprint().clone();
        assert_ne!(expected_alpha, expected_beta);
        router
            .register(Arc::new(DeclaredRunner {
                name: "declared",
                workload_types: vec![first, second],
            }))
            .unwrap();

        let capabilities = router.capabilities();
        let types = capabilities.runners()[0].workload_types();
        assert_eq!(
            types
                .iter()
                .map(|t| (t.kind(), t.fingerprint().clone()))
                .collect::<Vec<_>>(),
            vec![("Alpha", expected_alpha), ("Beta", expected_beta)]
        );
        assert_eq!(capabilities.schemas().len(), 2);
    }

    #[test]
    fn registration_refuses_an_unusable_declaration() {
        let mut router = RunnerRouter::new();
        let reserved = decl("solti.io/v1", "Embedded");
        let bad_gvk = decl("not a gvk", "Kind");

        for (name, unusable, needle) in [
            ("reserved", reserved, "reserved"),
            ("bad-gvk", bad_gvk, "not a gvk"),
        ] {
            let error = router
                .register(Arc::new(DeclaredRunner {
                    name,
                    workload_types: vec![unusable],
                }))
                .unwrap_err();
            assert!(
                matches!(&error, RouterError::InvalidWorkloadType { runner, .. } if runner == name),
                "{error:?}"
            );
            assert!(error.to_string().contains(needle), "{error}");
        }
        assert!(router.capabilities().runners().is_empty());
    }

    #[test]
    fn registration_refuses_over_64_runners_and_over_256_distinct_fingerprints() {
        let mut router = RunnerRouter::new();
        for index in 0..solti_model::MAX_AGENT_RUNNERS {
            router
                .register(Arc::new(DeclaredRunner {
                    name: Box::leak(format!("runner-{index}").into_boxed_str()),
                    workload_types: vec![decl(EXAMPLE, "Shared")],
                }))
                .unwrap();
        }
        let error = router
            .register(Arc::new(DeclaredRunner {
                name: "one-too-many",
                workload_types: vec![decl(EXAMPLE, "Shared")],
            }))
            .unwrap_err();
        assert!(
            matches!(&error, RouterError::InvalidCapability { runner, .. } if runner == "one-too-many"),
            "{error:?}"
        );
        assert_eq!(
            router.capabilities().runners().len(),
            solti_model::MAX_AGENT_RUNNERS
        );

        let distinct = |from: usize, to: usize| -> Vec<WorkloadDecl> {
            (from..to)
                .map(|index| {
                    WorkloadDecl::new(
                        EXAMPLE,
                        format!("Kind{index}"),
                        &schema(&format!("schema-{index}")),
                        |_| Ok(()),
                    )
                })
                .collect()
        };
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(DeclaredRunner {
                name: "full",
                workload_types: distinct(0, solti_model::MAX_AGENT_WORKLOAD_SCHEMAS),
            }))
            .unwrap();
        // Same fingerprints again are not new schemas: still accepted.
        router
            .register(Arc::new(DeclaredRunner {
                name: "same-schemas",
                workload_types: vec![WorkloadDecl::new(
                    EXAMPLE,
                    "Again",
                    &schema("schema-0"),
                    |_| Ok(()),
                )],
            }))
            .unwrap();
        let error = router
            .register(Arc::new(DeclaredRunner {
                name: "over",
                workload_types: distinct(
                    solti_model::MAX_AGENT_WORKLOAD_SCHEMAS,
                    solti_model::MAX_AGENT_WORKLOAD_SCHEMAS + 1,
                ),
            }))
            .unwrap_err();
        assert!(
            matches!(&error, RouterError::InvalidCapability { runner, .. } if runner == "over"),
            "{error:?}"
        );
        assert_eq!(router.capabilities().runners().len(), 2);
    }

    #[test]
    fn first_matching_runner_wins_in_registration_order() {
        let mut router = RunnerRouter::new();
        for (name, zone) in [("first", "eu"), ("second", "us")] {
            router
                .register_with_labels(
                    Arc::new(DeclaredRunner {
                        name,
                        workload_types: vec![decl(EXAMPLE, "Shared")],
                    }),
                    labels("zone", zone),
                )
                .unwrap();
        }
        let task = mk_task(TaskWorkload::Routed(routed(EXAMPLE, "Shared", json!({}))));

        assert_eq!(router.pick(&task).unwrap().name(), "first");
        assert_eq!(router.runner_name(&task).unwrap(), "first");
        let on_second = with_selector(task.clone(), selector("zone", "us"));
        assert_eq!(router.pick(&on_second).unwrap().name(), "second");
        let nowhere = with_selector(task, selector("zone", "asia"));
        assert!(matches!(
            router.pick(&nowhere),
            Err(RouterError::NoRunner { .. })
        ));

        let mut reversed = RunnerRouter::new();
        for (name, zone) in [("second", "us"), ("first", "eu")] {
            reversed
                .register_with_labels(
                    Arc::new(DeclaredRunner {
                        name,
                        workload_types: vec![decl(EXAMPLE, "Shared")],
                    }),
                    labels("zone", zone),
                )
                .unwrap();
        }
        let task = mk_task(TaskWorkload::Routed(routed(EXAMPLE, "Shared", json!({}))));
        assert_eq!(reversed.pick(&task).unwrap().name(), "second");
    }

    fn two_runners_one_gvk() -> RunnerRouter {
        let mut router = RunnerRouter::new();
        router
            .register_with_labels(
                Arc::new(DeclaredRunner {
                    name: "strict",
                    workload_types: vec![rejecting(EXAMPLE, "Shared", "strict says no")],
                }),
                labels("zone", "eu"),
            )
            .unwrap();
        router
            .register_with_labels(
                Arc::new(DeclaredRunner {
                    name: "lenient",
                    workload_types: vec![decl(EXAMPLE, "Shared")],
                }),
                labels("zone", "us"),
            )
            .unwrap();
        router
    }

    #[test]
    fn validate_uses_the_first_matching_runners_check() {
        let router = two_runners_one_gvk();
        let task = mk_task(TaskWorkload::Routed(routed(EXAMPLE, "Shared", json!({}))));

        let error = router.validate(task.spec()).unwrap_err();
        assert!(
            matches!(
                &error,
                RouterError::InvalidWorkload { runner, api_version, kind, reason }
                    if runner == "strict" && api_version == EXAMPLE && kind == "Shared"
                        && reason == "strict says no"
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("workload {EXAMPLE}/Shared rejected by runner 'strict': strict says no")
        );

        let on_second = with_selector(task, selector("zone", "us"));
        router.validate(on_second.spec()).unwrap();
    }

    #[test]
    fn validate_reports_no_runner_for_an_undeclared_type() {
        let router = two_runners_one_gvk();
        let other = routed(EXAMPLE, "Undeclared", json!({}));

        assert!(matches!(
            router.validate_workload(&other, None),
            Err(RouterError::NoRunner { api_version, kind })
                if api_version == EXAMPLE && kind == "Undeclared"
        ));
        let shared = routed(EXAMPLE, "Shared", json!({}));
        assert!(matches!(
            router.validate_workload(&shared, Some(&selector("zone", "asia"))),
            Err(RouterError::NoRunner { .. })
        ));
        assert!(matches!(
            RunnerRouter::new().validate_workload(&shared, None),
            Err(RouterError::NoRunner { .. })
        ));
    }

    #[test]
    fn validate_rejects_an_embedded_workload() {
        let router = two_runners_one_gvk();
        let task = mk_task(TaskWorkload::Embedded(
            EmbeddedSpec::new("test-revision").unwrap(),
        ));

        assert!(matches!(
            router.validate(task.spec()),
            Err(RouterError::EmbeddedWorkload)
        ));
        assert!(matches!(
            router.catalog().validate(task.spec()),
            Err(RouterError::EmbeddedWorkload)
        ));
    }

    #[test]
    fn catalog_validate_workload_matches_the_router() {
        let router = two_runners_one_gvk();
        let catalog = router.catalog();
        let shared = routed(EXAMPLE, "Shared", json!({}));
        let task = mk_task(TaskWorkload::Routed(shared.clone()));

        assert_eq!(
            catalog
                .validate_workload(&shared, None)
                .unwrap_err()
                .to_string(),
            router
                .validate_workload(&shared, None)
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            catalog.validate(task.spec()).unwrap_err().to_string(),
            router.validate(task.spec()).unwrap_err().to_string()
        );
        catalog
            .validate_workload(&shared, Some(&selector("zone", "us")))
            .unwrap();
        assert!(matches!(
            catalog.validate_workload(&routed(EXAMPLE, "Undeclared", json!({})), None),
            Err(RouterError::NoRunner { .. })
        ));
    }

    #[test]
    fn a_derived_check_names_the_gvk_once_in_the_router_error() {
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(DeclaredRunner {
                name: "resizer",
                workload_types: vec![WorkloadDecl::of::<Resize>()],
            }))
            .unwrap();

        let bad = routed(EXAMPLE, "Resize", json!({ "width": "wide" }));
        let error = router.validate_workload(&bad, None).unwrap_err();
        let text = error.to_string();
        assert_eq!(text.matches(EXAMPLE).count(), 1, "{text}");
        assert_eq!(text.matches("Resize").count(), 1, "{text}");
        assert!(
            text.starts_with(&format!(
                "workload {EXAMPLE}/Resize rejected by runner 'resizer': spec.width: "
            )),
            "{text}"
        );
        router
            .validate_workload(&routed(EXAMPLE, "Resize", json!({ "width": 3 })), None)
            .unwrap();
    }
}
