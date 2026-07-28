# solti-runner

`solti-runner` is the plugin boundary between execution backends and the supervisor.

Implement `Runner` for a backend and register it in a `RunnerRouter`; the router routes each `Task` by workload GVK and optional labels, then builds a `taskvisor::TaskRef`.
`solti-exec` implements `Runner` for subprocesses; `solti-core` consumes the router.

The crate does not execute or supervise tasks; Taskvisor owns execution and lifecycle.
The crate has no optional features.

## Quick start

Implement `Runner`, register it, and build a Taskvisor task:

```rust,no_run
use std::sync::Arc;

use solti_model::{Task, WorkloadTypeMeta, WORKLOAD_API_VERSION};
use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

struct EchoRunner;

impl Runner for EchoRunner {
    fn name(&self) -> &str {
        "echo"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess")
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

fn build(resource: &Task) -> Result<TaskRef, Box<dyn std::error::Error>> {
    let mut router = RunnerRouter::new();
    router.register(Arc::new(EchoRunner))?;
    Ok(router.build(resource)?)
}
```

The example declares the built-in `Subprocess` GVK.
`solti-exec` provides its production implementation.

## What it does

- shared environment, metrics, and output dependencies for every runner;
- one plugin contract for built-in and application-defined backends;
- deterministic routing by GVK, selector, and registration order;
- validated capability metadata for control-plane discovery.

## Inputs and outputs

| API                                   | Input                               | Output                                  |
|---------------------------------------|-------------------------------------|-----------------------------------------|
| `register`                            | `Arc<dyn Runner>`                   | Validated runner entry                  |
| `register_with_labels`                | Runner and static `Labels`          | Labeled runner entry                    |
| `pick`                                | `Task`                              | First matching `Runner`                 |
| `build`                               | `Task`                              | `taskvisor::TaskRef`                    |
| `capabilities`                        | Registered entries                  | Owned `AgentCapabilities` snapshot      |
| `merge_env`                           | `TaskEnv` and `RunnerEnv`           | Sorted process environment              |
| `OutputSink::stdout_line`             | `Bytes`                             | `OutputEvent::Chunk`                    |
| `MetricsBackend::record_runner_error` | Runner and error labels             | Backend-specific metric update          |

## Routing

```text
Task
  │
  ├── workload GVK ───────┐
  └── runnerSelector ─────┤
                          ▼
                    RunnerRouter
                          │ first match in registration order
                          ▼
        Runner::build_task(Task, RunId, BuildContext)
                          │
                          ▼
                 taskvisor::TaskRef
```

Routing uses only workload GVK and `runnerSelector`.
It does not inspect the workload payload.

The router applies these rules in order:

1. Reject `TaskWorkload::Embedded`.
2. Keep runners that declared the exact `apiVersion` and `kind`.
3. Apply `runnerSelector` to static runner labels when present.
4. Select the first remaining runner in registration order.

| Workload variation | Routing behavior                                         |
|--------------------|----------------------------------------------------------|
| Built-in workload  | A runner declares its built-in GVK                       |
| Custom workload    | An `ExtensionWorkload` and runner share a custom GVK     |
| Alternate backend  | Several runners share a GVK and labels select one        |
| Embedded workload  | Bypasses the router because its `TaskRef` already exists |

## Registration and capabilities

Registration validates and snapshots the runner declaration.

- Runner names are unique.
- Runner names use Kubernetes label-value rules.
- Static labels use Kubernetes label rules.
- At least one workload GVK is required.
- Duplicate workload GVKs are rejected.
- The built-in `Embedded` GVK is rejected.

`RunnerRouter::capabilities()` returns an owned snapshot.
Runner entries remain in routing priority order.
Workload GVKs inside each entry use canonical order.

## Build contract

`RunnerRouter::build` allocates a `RunId`.
Its format is `{runner}-{slot}-{seq}`.
The sequence comes from one process-global counter initialized to `1`.

The router passes the resource, run ID, and `BuildContext` to `Runner::build_task`.
The returned `TaskRef` must use the allocated run ID as its name.
A mismatch returns `RouterError::RunIdMismatch`.

`build_task` constructs a task but does not start it.
Submission can still be rejected after construction.
A returned task can run more than once under its Taskvisor restart policy.
Attempt-scoped resources belong inside the task body.

## Build context

| Value              | Default                   | Replacement API                         |
|--------------------|---------------------------|-----------------------------------------|
| `RunnerEnv`        | Empty                     | `with_env`                              |
| `MetricsHandle`    | `NoOpMetrics`             | `with_metrics`                          |
| Output publisher   | Output disabled           | `with_output_publisher`                 |

`RunnerRouter::with_context` installs one context for all registered runners.
`RunnerRouter::with_output_publisher` replaces only the output producer.

## Environment

`merge_env` produces a `BTreeMap<String, String>`:

```rust
use solti_model::TaskEnv;
use solti_runner::{RunnerEnv, merge_env};

let mut task = TaskEnv::new();
task.push("PATH", "/task/bin");
task.push("TASK_ONLY", "yes");

let mut runner = RunnerEnv::new();
runner.push("PATH", "/runner/bin");

let merged = merge_env(&task, &runner);
assert_eq!(merged["PATH"], "/runner/bin");
assert_eq!(merged["TASK_ONLY"], "yes");
```

Runner values override task values.
Within each input, the last value for a key wins.
The returned map is sorted by key.

## Output

```text
runner attempt
      │
      ▼
OutputPublisher::sink_for(task, generation, attempt)
      │
      ├── None ──► output disabled
      └── OutputSink ──► stdout / stderr chunks ──► composition layer
```

`OutputSink` is a write-only producer:

```rust
use std::sync::mpsc;

use bytes::Bytes;
use solti_model::OutputEvent;
use solti_runner::OutputSink;

let (sender, receiver) = mpsc::channel();
let sink = OutputSink::new(4, 2, move |event| {
    sender.send(event).unwrap();
});

sink.stdout_line(Bytes::from_static(b"ready"));
assert!(matches!(receiver.recv().unwrap(), OutputEvent::Chunk(_)));
```

- A sink belongs to one generation and attempt.
- Stdout and stderr have independent sequences starting at `0`.
- Clones share both sequence counters.
- `Bytes` payloads are forwarded without conversion.
- Publishing is synchronous.
- The callback must not block runner execution.
- Runners cannot subscribe to output or publish lifecycle markers.

## Metrics

Runners report backend setup and cleanup failures:

```rust
use solti_runner::{RunnerErrorKind, RunnerType, noop_metrics};

let metrics = noop_metrics();
metrics.record_runner_error(
    RunnerType::Subprocess,
    RunnerErrorKind::SpawnFailed,
);
```

`NoOpMetrics` is the default backend.
`solti-prometheus` provides a Prometheus implementation.
Task lifecycle metrics come from Taskvisor events.

Built-in metric variants use stable labels.
`Custom` variants use the application-provided string unchanged.
The application controls cardinality for custom labels.

## Errors

`RouterError` describes registration, selection, and construction failures:

| Variant             | Cause                                          |
|---------------------|------------------------------------------------|
| `DuplicateRunner`   | Runner name already registered                 |
| `InvalidLabels`     | Static labels failed model validation          |
| `InvalidCapability` | Runner name or workload declaration invalid    |
| `EmbeddedWorkload`  | Embedded workload sent to the router           |
| `NoRunner`          | No runner matched the GVK and selector         |
| `Build`             | Selected runner returned `RunnerError`         |
| `RunIdMismatch`     | Returned task used a different name            |

`RunnerError` is returned by a concrete runner:

| Variant               | Cause                                      |
|-----------------------|--------------------------------------------|
| `UnsupportedWorkload` | Runner received an unsupported GVK         |
| `InvalidSpec`         | Workload desired state is invalid          |
| `Internal`            | Runner could not construct the task        |
