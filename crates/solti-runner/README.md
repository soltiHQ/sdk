# solti-runner

> Runner plugin interface for Solti tasks.

`solti-runner` sits between `solti-model` and `solti-core`.
It does not run tasks by itself. It defines how a concrete backend, called a runner, turns a `TaskSpec` into a `taskvisor::TaskRef`.

Use it when you want one agent binary to support different execution backends: subprocesses, containers, WASM modules, or your own runner.

## The switch you stop writing

Without a router, each agent has to decide how to run a task:

```rust,ignore
match spec.kind() {
    TaskKind::Subprocess(_) => build_subprocess_task(spec)?,
    TaskKind::Wasm(_) => build_wasm_task(spec)?,
    TaskKind::Container(_) => build_container_task(spec)?,
    TaskKind::Embedded => return Err("embedded tasks are already built"),
}
```

With `solti-runner`, each backend implements `Runner`. The router picks the first matching runner:

```rust,no_run
use std::sync::Arc;
use solti_runner::RunnerRouter;

# use solti_runner::{BuildContext, Runner, RunnerError};
# use solti_model::{TaskKind, TaskSpec};
# use taskvisor::TaskRef;
# struct MyRunner;
# impl Runner for MyRunner {
#     fn name(&self) -> &'static str { "my-runner" }
#     fn supports(&self, spec: &TaskSpec) -> bool { matches!(spec.kind(), TaskKind::Subprocess(_)) }
#     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
# }
# fn wire(spec: &TaskSpec) -> Result<TaskRef, RunnerError> {
let mut router = RunnerRouter::new();
router.register(Arc::new(MyRunner));

let task = router.build(spec)?;
# Ok(task)
# }
```

## Quick Start

Implement `Runner`, register it, and ask the router to build a task:

```rust,no_run
use std::sync::Arc;

use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec};
use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

struct EchoRunner;

impl Runner for EchoRunner {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn supports(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind(), TaskKind::Subprocess(_))
    }

    fn build_task(&self, spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        let run_id = self.build_run_id(spec.slot().as_ref());
        Ok(TaskFn::arc(run_id.into_name(), |_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        }))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kind = TaskKind::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into()],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("hello", kind, 5_000u64).build()?;

    let mut router = RunnerRouter::new();
    router.register(Arc::new(EchoRunner));

    let task = router.build(&spec)?;
    let _ = task;
    Ok(())
}
```

In a real agent, `solti-exec` provides the subprocess runner. You only implement `Runner` when you build a new backend.

## Why solti-runner?

- **One plugin shape**: every backend implements the same `Runner` trait.
- **Simple routing**: the first registered runner that supports the spec is used.
- **Label selection**: a task can ask for a runner with labels, such as `zone=eu` or `gpu=true`.
- **Shared context**: runners receive env, metrics, and output registry handles through `BuildContext`.
- **Live output**: runners can push stdout and stderr lines into an `OutputRegistry`.
- **Low-cardinality metrics**: metrics labels are enums, not free-form error strings.

## When to Use It

Use this crate when you write:

- a new runner backend;
- an agent binary that registers several runners;
- tests for routing behavior;
- a metrics backend for task execution;
- live-tail output support for a runner.

Do not use it for `TaskKind::Embedded`. Embedded tasks already come as a `TaskRef`; submit them with `SupervisorApi::submit_with_task`.

## Core Model

```text
TaskSpec
  |
  v
RunnerRouter
  |
  | checks Runner::supports(spec)
  | checks runner labels, if the spec has a selector
  v
Runner::build_task(spec, BuildContext)
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
# use solti_model::{TaskKind, TaskSpec};
# use taskvisor::TaskRef;
# struct MyRunner;
# impl Runner for MyRunner {
#     fn name(&self) -> &'static str { "gpu-runner" }
#     fn supports(&self, _spec: &TaskSpec) -> bool { true }
#     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> { todo!() }
# }
let mut labels = Labels::new();
labels.insert("gpu", "true");

let mut router = RunnerRouter::new();
router.register_with_labels(Arc::new(MyRunner), labels);
```

If a `TaskSpec` has a `RunnerSelector`, the router only keeps runners whose labels match that selector.

## Output Streaming

`OutputRegistry` is a live output hub. A runner gets an `OutputSink` for one task attempt and pushes lines into it:

```rust
use bytes::Bytes;
use solti_model::{OutputEvent, TaskId};
use solti_runner::OutputRegistry;

# async fn demo() {
let registry = OutputRegistry::new(64);
let task_id = TaskId::from("task-1");

let sink = registry.sink_for(task_id.clone(), 1);
let mut rx = registry.subscribe(&task_id).unwrap();

sink.stdout_line(Bytes::from_static(b"hello"));

match rx.recv().await.unwrap() {
    OutputEvent::Chunk(chunk) => assert_eq!(&chunk.line[..], b"hello"),
    other => panic!("unexpected event: {other:?}"),
}
# }
```

Channels are `tokio::sync::broadcast`. A slow subscriber does not block the runner. It may receive a lag signal and then continue from newer events.

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
| Output        | `OutputRegistry`, `OutputSink`                   |
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
- `OutputRegistry` channels are per `TaskId` and reused across retries.
- `BuildContext::default()` uses empty env, `NoOpMetrics`, and an empty `OutputRegistry`.
