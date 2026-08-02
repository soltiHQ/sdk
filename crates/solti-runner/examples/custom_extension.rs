//! # Custom extension runner
//!
//! This example routes one application-defined workload.
//!
//! It shows how to:
//! - define an `ExtensionWorkload` with a custom GVK;
//! - register two runners for the same GVK;
//! - select one runner through `runnerSelector`;
//! - inspect the registered capability snapshot;
//! - use the allocated `RunId` as the returned `TaskRef` name.
//!
//! Routing uses only the workload GVK and static runner labels.
//! The selected runner validates the application-owned `spec`.
//! `RunnerRouter::build` constructs the task but does not start it.
//!
//! Run with `cargo run -p solti-runner --example custom_extension`.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use solti_model::{
    ExtensionWorkload, LabelSelector, Labels, Task, TaskSpec, TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

const API_VERSION: &str = "media.example.io/v1";
const KIND: &str = "ImageResize";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageResizeSpec {
    source: String,
    width: u32,
}

struct ImageResizeRunner {
    name: &'static str,
    accelerator: &'static str,
}

impl Runner for ImageResizeRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new(API_VERSION, KIND).expect("valid extension GVK")]
    }

    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        _ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let workload = task.spec().workload();
        let TaskWorkload::Extension(extension) = workload else {
            return Err(unsupported_workload(self.name, workload));
        };
        if extension.api_version() != API_VERSION || extension.kind() != KIND {
            return Err(unsupported_workload(self.name, workload));
        }

        let spec: ImageResizeSpec = serde_json::from_value(extension.spec().clone())
            .map_err(|error| RunnerError::InvalidSpec(error.to_string()))?;
        if spec.source.trim().is_empty() {
            return Err(RunnerError::InvalidSpec("source must not be empty".into()));
        }
        if spec.width == 0 {
            return Err(RunnerError::InvalidSpec(
                "width must be greater than zero".into(),
            ));
        }

        let accelerator = self.accelerator;
        let source: Arc<str> = spec.source.into();
        let width = spec.width;
        Ok(TaskFn::arc(
            run_id.name().to_owned(),
            move |_ctx: TaskContext| {
                let source = Arc::clone(&source);
                async move {
                    println!("resize {source} to {width}px with {accelerator}");
                    Ok::<(), TaskError>(())
                }
            },
        ))
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    println!("Registered capabilities:");
    for runner in capabilities.runners() {
        let workload = &runner.workload_types()[0];
        let accelerator = runner.labels().get("accelerator").unwrap_or("unknown");
        println!(
            "  {}: {}/{} accelerator={accelerator}",
            runner.name(),
            workload.api_version(),
            workload.kind(),
        );
    }

    let workload = TaskWorkload::Extension(ExtensionWorkload::new(
        API_VERSION,
        KIND,
        json!({
            "source": "cover.png",
            "width": 1280
        }),
    )?);
    let spec = TaskSpec::builder("image-resize", workload, 30_000_u64)
        .runner_selector(LabelSelector::from_labels(labels("gpu")))
        .build()?;
    let task = Task::new("resize-cover", spec)?;

    let selected = router.pick(&task)?;
    println!("Selected runner: {}", selected.name());
    assert_eq!(selected.name(), "resize-gpu");

    let task_ref = router.build(&task)?;
    println!("Built TaskRef: {}", task_ref.name());
    assert!(task_ref.name().starts_with("resize-gpu-image-resize-"));

    Ok(())
}
