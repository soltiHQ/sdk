# solti-runner

> Runner plugin interface for Solti tasks.

`solti-runner` sits between `solti-model` and `solti-core`.
It does not run tasks by itself. It defines how a concrete backend, called a runner, turns a `Task` resource into a `taskvisor::TaskRef`.

Use it when you want one agent binary to support different execution backends: subprocesses, containers, WASM modules, or your own runner.

## The switch you stop writing

Without a router, each agent has to decide how to run a task:

```rust,ignore
match task.spec().workload() {
    TaskWorkload::Subprocess(_) => build_subprocess_task(task)?,
    TaskWorkload::Wasm(_) => build_wasm_task(task)?,
    TaskWorkload::Container(_) => build_container_task(task)?,
    TaskWorkload::Extension(_) => build_extension_task(task)?,
    TaskWorkload::Embedded(_) => return Err("embedded tasks are already built"),
}
```

With `solti-runner`, each backend implements `Runner`. The router picks the first matching runner:

```rust,no_run
use std::sync::Arc;
use solti_runner::RunnerRouter;

# use solti_runner::{BuildContext, Runner, RunnerError};
# use solti_model::{Task, TaskWorkload};
# use taskvisor::TaskRef;
# struct MyRunner;
# impl Runner for MyRunner {
#     fn name(&self) -> &'static str { "my-runner" }
#     fn supports(&self, workload: &TaskWorkload) -> bool { matches!(workload, TaskWorkload::Subprocess(_)) }
#     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
# }
# fn wire(resource: &Task) -> Result<TaskRef, RunnerError> {
let mut router = RunnerRouter::new();
router.register(Arc::new(MyRunner));

let task = router.build(resource)?;
# Ok(task)
# }
```

## Quick Start

Implement `Runner`, register it, and ask the router to build a task:

```rust,no_run
use std::sync::Arc;

use solti_model::{Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskSpec, TaskWorkload};
use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

struct EchoRunner;

impl Runner for EchoRunner {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn supports(&self, workload: &TaskWorkload) -> bool {
        matches!(workload, TaskWorkload::Subprocess(_))
    }

    fn build_task(&self, task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let run_id = self.build_run_id(task.slot().as_ref());
        Ok(TaskFn::arc(run_id.into_name(), |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        }))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into()],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("hello", workload, 5_000u64).build()?;
    let resource = Task::new("hello", spec)?;

    let mut router = RunnerRouter::new();
    router.register(Arc::new(EchoRunner));

    let task = router.build(&resource)?;
    let _ = task;
    Ok(())
}
```

In a real agent, `solti-exec` provides the subprocess runner. You only implement `Runner` when you build a new backend.

## Why solti-runner?

- **One plugin shape**: every backend implements the same `Runner` trait.
- **Simple routing**: the first registered runner that supports the spec is used.
- **Label selection**: a task can ask for a runner with labels, such as `zone=eu` or `gpu=true`.
- **Shared context**: runners receive env, metrics, and an output producer capability through `BuildContext`.
- **Live output**: runners push stdout and stderr through attempt-scoped `OutputSink` values without gaining subscription or lifecycle control.
- **Low-cardinality metrics**: metrics labels are enums, not free-form error strings.

## When to Use It

Use this crate when you write:

- a new runner backend;
- an agent binary that registers several runners;
- tests for routing behavior;
- a metrics backend for task execution;
- live-tail output support for a runner.

Do not route `TaskWorkload::Embedded`. Embedded resources already come with a `TaskRef`; pass both to `SupervisorApi::create_with_task` or `SupervisorApi::apply_with_task`.

## Core Model

```text
Task
  |
  v
RunnerRouter
  |
  | checks Runner::supports(task.spec.workload)
  | checks runner labels, if the task spec has a selector
  v
Runner::build_task(task, BuildContext)
  |
  v
taskvisor::TaskRef
```

Runners are checked in registration order. If more than one runner matches, the first one wins.

## Routing With Labels

Register runners with labels:

```rust,no_run
use std::sync::Arc;
use solti_model::Labels;
use solti_runner::RunnerRouter;

# use solti_runner::{BuildContext, Runner, RunnerError};
# use solti_model::{Task, TaskWorkload};
# use taskvisor::TaskRef;
# struct MyRunner;
# impl Runner for MyRunner {
#     fn name(&self) -> &'static str { "gpu-runner" }
#     fn supports(&self, _workload: &TaskWorkload) -> bool { true }
#     fn build_task(&self, _task: &Task, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
# }
let mut labels = Labels::new();
labels.insert("gpu", "true");

let mut router = RunnerRouter::new();
router.register_with_labels(Arc::new(MyRunner), labels);
```

If a task spec has a `RunnerSelector`, the router only keeps runners whose labels match that selector.

## Output Streaming

`OutputPublisher` is the runner-facing producer port. A runner obtains an
`OutputSink` inside each attempt and pushes lines into it:

```rust
use bytes::Bytes;
use solti_model::TaskId;
use solti_runner::BuildContext;

let context = BuildContext::default();
let task_id = TaskId::from("task-1");

if let Some(sink) = context.output_publisher().sink_for(&task_id, 1, 1) {
    sink.stdout_line(Bytes::from_static(b"hello"));
}
```

`BuildContext::default()` uses a no-op publisher. `solti-core` replaces it with
its private live-output hub when constructing `SupervisorApi`. A custom
standalone composition can inject its own `OutputPublisher` with
`BuildContext::with_output_publisher()`.

The producer call is synchronous and must remain non-blocking. The standard core
implementation is lossy: slow consumers do not block runner execution.

## Metrics

Runners record task execution through a `MetricsBackend`:

```rust
use solti_runner::{MetricOutcome, RunnerType, noop_metrics};

let metrics = noop_metrics();
metrics.record_task_started(RunnerType::Subprocess);
metrics.record_task_completed(RunnerType::Subprocess, MetricOutcome::Success, 42);
```

The default backend is `NoOpMetrics`. Production agents can use `solti-prometheus`.

## Main Types

| Area          | Types                                            |
|---------------|--------------------------------------------------|
| Runner plugin | `Runner`, `RunnerRouter`                         |
| Build data    | `BuildContext`                                   |
| Output        | `OutputPublisher`, `OutputPublisherHandle`, `OutputSink` |
| Run identity  | `RunId`, `make_run_id`                           |
| Metrics       | `MetricsBackend`, `MetricsHandle`, `NoOpMetrics` |
| Metric labels | `RunnerType`, `MetricOutcome`, `RunnerErrorKind` |
| Errors        | `RunnerError`                                    |

## Error Handling

`RunnerError` covers routing and task-build failures:

| Variant           | Meaning                               |
|-------------------|---------------------------------------|
| `NoRunner`        | No registered runner matched the spec |
| `UnsupportedKind` | A runner rejected this task kind      |
| `InvalidSpec`     | The spec is not valid for this runner |
| `MissingField`    | A required field is missing           |
| `Internal`        | Runner-specific failure               |
| `Io`              | I/O failed while building the task    |

The enum is `#[non_exhaustive]`, so match it with a wildcard arm.

## Notes

- `RunId` is `{runner}-{slot}-{seq}`.
- The `seq` is process-global and starts at `1`.
- `OutputSink` sequence counters are attempt-scoped and independent for stdout and stderr.
- `BuildContext::default()` uses empty env, `NoOpMetrics`, and a no-op output publisher.
