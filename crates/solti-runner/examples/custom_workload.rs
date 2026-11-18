//! # Custom workload runner
//!
//! This example routes one application-defined workload.
//!
//! It shows how to:
//! - define a body type with `#[derive(Deserialize, JsonSchema)]` and `WorkloadBody`;
//! - declare it with `WorkloadDecl::of`, which derives the schema and the body check;
//! - register two runners for the same GVK;
//! - select one runner through `runnerSelector`;
//! - check a body with the runner that would build it;
//! - inspect the registered capability snapshot, with schema fingerprints;
//! - inspect the allocated `RunId` returned beside the executable task.
//!
//! Routing uses only the workload GVK and static runner labels.
//! The first matching runner validates the body and then builds the task.
//! `RunnerRouter::build` constructs the task but does not start it.
//!
//! Run with `cargo run -p solti-runner --example custom_workload`.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use solti_model::{
    LabelSelector, Labels, Task, TaskSpec, TaskWorkload, Workload, WorkloadBody, WorkloadDecl,
};
use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

const API_VERSION: &str = "media.example.io/v1";
const KIND: &str = "ImageResize";

const FLOW: &str = r#"
solti-runner: custom workload routing

  resize-cpu + accelerator=cpu ─┐
                                ├──► register ──► RunnerRouter
  resize-gpu + accelerator=gpu ─┘                     │
                                                      ├──► capability snapshot
  Task                                                │
    ├── GVK: media.example.io/v1, ImageResize ────────┤
    └── runnerSelector: accelerator=gpu ──────────────┤
                                                      │ first matching runner
                                                      ▼
                                                resize-gpu
                                                      │ Task + RunId + BuildContext + cancellation
                                                      ▼
                                            Runner::build_task
                                                      │ decode application spec
                                                      ▼
                                             taskvisor::TaskRef
                                                      │ router pairs it with RunId
                                                      ▼
                                                  BuiltTask

  Routing reads only the GVK and selector.
  Validation runs the first matching runner's declared check on the body.
  Building creates a task; it does not start it.
"#;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageResizeSpec {
    source: String,
    width: u32,
}

impl WorkloadBody for ImageResizeSpec {
    const API_VERSION: &'static str = "media.example.io/v1";
    const KIND: &'static str = "ImageResize";

    fn validate(&self) -> Result<(), String> {
        if self.source.trim().is_empty() {
            return Err("source must not be empty".into());
        }
        if self.width == 0 {
            return Err("width must be greater than zero".into());
        }
        Ok(())
    }
}

struct ImageResizeRunner {
    name: &'static str,
    accelerator: &'static str,
}

#[solti_runner::async_trait]
impl Runner for ImageResizeRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadDecl> {
        vec![WorkloadDecl::of::<ImageResizeSpec>()]
    }

    async fn build_task(
        &self,
        task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let workload = task.spec().workload();
        let TaskWorkload::Routed(routed) = workload else {
            return Err(unsupported_workload(self.name, workload));
        };
        let spec = routed
            .decode::<ImageResizeSpec>()
            .map_err(|error| match error.kind() {
                solti_model::WorkloadBodyErrorKind::TypeMismatch { .. } => {
                    unsupported_workload(self.name, workload)
                }
                _ => RunnerError::InvalidSpec(error.detail()),
            })?;

        let accelerator = self.accelerator;
        let source: Arc<str> = spec.source.into();
        let width = spec.width;
        Ok(TaskFn::arc(move |_ctx: TaskContext| {
            let source = Arc::clone(&source);
            async move {
                println!("resize {source} to {width}px with {accelerator}");
                Ok::<(), TaskError>(())
            }
        }))
    }
}

fn unsupported_workload(runner: &str, workload: &TaskWorkload) -> RunnerError {
    RunnerError::UnsupportedWorkload {
        runner: runner.to_owned(),
        api_version: workload.api_version().to_owned(),
        kind: workload.kind().to_owned(),
    }
}

fn labels(accelerator: &str) -> Labels {
    let mut labels = Labels::new();
    labels.insert("accelerator", accelerator);
    labels
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{FLOW}");
    println!(
        "[purpose] Add an application-owned workload and choose between backends without changing solti-runner."
    );

    println!("[registration] Register CPU and GPU runners for the same custom GVK.");
    println!("[registration] Runner names, labels, and workload declarations are validated here.");
    let mut router = RunnerRouter::new();
    router.register_with_labels(
        Arc::new(ImageResizeRunner {
            name: "resize-cpu",
            accelerator: "cpu",
        }),
        labels("cpu"),
    )?;
    router.register_with_labels(
        Arc::new(ImageResizeRunner {
            name: "resize-gpu",
            accelerator: "gpu",
        }),
        labels("gpu"),
    )?;

    let capabilities = router.capabilities();
    println!("[capabilities] The immutable discovery snapshot advertises:");
    for runner in capabilities.runners() {
        let workload = &runner.workload_types()[0];
        let accelerator = runner.labels().get("accelerator").unwrap_or("unknown");
        println!(
            "      runner={} apiVersion={} kind={} fingerprint={} accelerator={accelerator}",
            runner.name(),
            workload.api_version(),
            workload.kind(),
            workload.fingerprint().as_str(),
        );
    }

    let body = Workload::new(
        API_VERSION,
        KIND,
        json!({
            "source": "cover.png",
            "width": 1280
        }),
    )?;
    let workload = TaskWorkload::Routed(body);
    let spec = TaskSpec::builder("image-resize", workload, 30_000_u64)
        .runner_selector(LabelSelector::from_labels(labels("gpu")))
        .build()?;
    let task = Task::new("resize-cover", spec)?;
    println!(
        "[resource] Task resize-cover requests apiVersion={API_VERSION}, kind={KIND}, selector accelerator=gpu."
    );

    let selected = router.pick(&task)?;
    println!(
        "[routing] Exact GVK and selector matching chooses runner={}.",
        selected.name(),
    );
    assert_eq!(selected.name(), "resize-gpu");

    println!("[validation] The first matching runner checks the body before it is stored.");
    router.validate(task.spec())?;
    let refused = Workload::new(
        API_VERSION,
        KIND,
        json!({ "source": "cover.png", "width": 0 }),
    )?;
    let error = router
        .validate_workload(&refused, Some(&LabelSelector::from_labels(labels("gpu"))))
        .expect_err("width 0 is refused");
    println!("[validation] A bad body is refused: {error}.");

    let built = router.build(&task).await?;
    println!("[build] The runner decodes the body again and builds the task.");
    println!(
        "[build] BuiltTask keeps the allocated RunId beside the executable task: {}.",
        built.name(),
    );
    assert!(built.name().starts_with("resize-gpu-image-resize-"));

    println!(
        "\nResult: routing and construction succeeded; the built task was not submitted or executed."
    );

    Ok(())
}
