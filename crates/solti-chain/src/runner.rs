//! Runner implementation for the chain extension workload.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use bytes::Bytes;
use solti_model::{Task, TaskId, WorkloadTypeMeta};
use solti_runner::{
    BuildContext, RouterError, RunId, Runner, RunnerCatalog, RunnerError, RunnerRouter,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tracing::{debug, instrument};

use crate::output::ChainOutput;
use crate::{CHAIN_API_VERSION, CHAIN_KIND, ChainSpec, FailureMode};

/// Runner that composes registered leaf workloads into one conditional chain.
///
/// A runner catalog is an immutable allowlist.
/// Take it before registering this runner; chain cannot route another chain recursively.
pub struct ChainRunner {
    name: String,
    catalog: RunnerCatalog,
}

impl ChainRunner {
    /// Creates a chain runner backed by the provided leaf-runner catalog.
    pub fn new(name: impl Into<String>, catalog: RunnerCatalog) -> Self {
        Self {
            name: name.into(),
            catalog,
        }
    }
}

impl Runner for ChainRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(CHAIN_API_VERSION, CHAIN_KIND)
                .expect("chain workload GVK is valid"),
        ]
    }

    #[instrument(
        level = "debug",
        skip(self, task, ctx),
        fields(task = %task.name(), generation = task.metadata().generation())
    )]
    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let spec = ChainSpec::from_workload(task.spec().workload())
            .map_err(|error| RunnerError::InvalidSpec(error.to_string()))?;
        let output = ChainOutput::new(
            Arc::clone(ctx.output_publisher()),
            task.name().clone(),
            task.metadata().generation(),
        );
        let child_ctx = ctx.clone().with_output_publisher(output.publisher());
        let plan = Arc::new(self.compile(task, &spec, &child_ctx)?);
        let attempts = Arc::new(AtomicU32::new(0));

        debug!(steps = plan.steps.len(), entry = %spec.entry(), "chain compiled");

        Ok(TaskFn::arc(
            run_id.name().to_owned(),
            move |ctx: TaskContext| {
                let plan = Arc::clone(&plan);
                let output = Arc::clone(&output);
                let attempt = attempts.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                async move {
                    let output = output.begin(attempt);
                    execute(plan, ctx, output.sink()).await
                }
            },
        ))
    }
}

impl ChainRunner {
    fn compile(
        &self,
        outer: &Task,
        spec: &ChainSpec,
        ctx: &BuildContext,
    ) -> Result<CompiledChain, RunnerError> {
        let indices = spec
            .steps()
            .iter()
            .enumerate()
            .map(|(index, step)| (step.name().clone(), index))
            .collect::<HashMap<_, _>>();

        let mut steps = Vec::with_capacity(spec.steps().len());
        for step in spec.steps() {
            let derived = derive_task(outer, step)?;
            let task = self.catalog.build(&derived, ctx).map_err(|error| {
                RunnerError::InvalidSpec(format!(
                    "chain step '{}' could not be built: {error}",
                    step.name()
                ))
            })?;
            let on_success = step.on_success().map(|next| indices[next]);
            let on_failure = step.on_failure().map(|transition| CompiledFailure {
                next: indices[transition.next()],
                mode: transition.mode(),
            });
            steps.push(CompiledStep {
                name: step.name().clone(),
                task,
                on_success,
                on_failure,
            });
        }

        Ok(CompiledChain {
            entry: indices[spec.entry()],
            steps,
        })
    }
}

fn derive_task(outer: &Task, step: &crate::ChainStep) -> Result<Task, RunnerError> {
    let (type_meta, metadata, outer_spec, status) = outer.clone().into_parts();
    let mut step_spec = outer_spec
        .with_workload(step.workload().clone())
        .without_runner_selector();
    if let Some(selector) = step.runner_selector() {
        step_spec = step_spec.with_runner_selector(selector.clone());
    }
    Task::from_parts(type_meta, metadata, step_spec, status)
        .map_err(|error| RunnerError::InvalidSpec(error.to_string()))
}

struct CompiledChain {
    entry: usize,
    steps: Vec<CompiledStep>,
}

struct CompiledStep {
    name: TaskId,
    task: TaskRef,
    on_success: Option<usize>,
    on_failure: Option<CompiledFailure>,
}

#[derive(Clone, Copy)]
struct CompiledFailure {
    next: usize,
    mode: FailureMode,
}

async fn execute(
    plan: Arc<CompiledChain>,
    ctx: TaskContext,
    output: Option<&solti_runner::OutputSink>,
) -> Result<(), TaskError> {
    let mut current = plan.entry;
    let mut preserved_failure = None;

    loop {
        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }
        let step = &plan.steps[current];
        marker(output, &step.name, "started", false);
        let result = match ctx.run_until_cancelled(step.task.spawn(ctx.child())).await {
            Ok(result) => result,
            Err(canceled) => Err(canceled),
        };

        match result {
            Ok(()) => {
                marker(output, &step.name, "succeeded", false);
                if let Some(next) = step.on_success {
                    current = next;
                    continue;
                }
                return preserved_failure.map_or(Ok(()), Err);
            }
            Err(error) if matches!(error, TaskError::Canceled) => {
                marker(output, &step.name, "canceled", true);
                return Err(error);
            }
            Err(error) => {
                marker(output, &step.name, "failed", true);
                let Some(transition) = step.on_failure else {
                    return Err(error);
                };
                if transition.mode == FailureMode::Preserve && preserved_failure.is_none() {
                    preserved_failure = Some(error);
                }
                current = transition.next;
            }
        }
    }
}

fn marker(output: Option<&solti_runner::OutputSink>, step: &TaskId, state: &str, error: bool) {
    let Some(output) = output else {
        return;
    };
    let line = Bytes::from(format!("[chain] step={step} state={state}"));
    if error {
        output.stderr_line(line);
    } else {
        output.stdout_line(line);
    }
}

/// Registers a chain runner after snapshotting every runner already registered.
///
/// Register all workloads allowed inside a chain before calling this helper.
/// Later registrations remain available to top-level Tasks but are intentionally absent from the chain's immutable allowlist.
///
/// # Errors
///
/// Returns [`RouterError`] when the runner name or capability conflicts with an existing registration.
pub fn register_chain_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
) -> Result<(), RouterError> {
    let runner = Arc::new(ChainRunner::new(name, router.catalog()));
    router.register(runner)
}
