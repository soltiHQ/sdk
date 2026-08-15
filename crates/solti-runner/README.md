# solti-runner

`solti-runner` is the plugin boundary between execution backends and the supervisor.

Implement `Runner` for a backend and register it in a `RunnerRouter`; the router routes each `Task` by workload GVK and optional labels, then returns a `BuiltTask` containing the allocated `RunId` and executable `taskvisor::TaskRef`.
`solti-exec` implements `Runner` for subprocesses; `solti-core` consumes the router.

The crate does not execute or supervise tasks; Taskvisor owns execution and lifecycle.
The crate has no optional features.

## Quick start

Implement `Runner`, register it, and build a Taskvisor task:

```rust,no_run
use std::sync::Arc;

use solti_model::{Task, WorkloadTypeMeta, WORKLOAD_API_VERSION};
use solti_runner::{BuildCancellation, BuildContext, BuildScope, BuiltTask, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

struct EchoRunner;

#[solti_runner::async_trait]
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

async fn build(resource: &Task) -> Result<BuiltTask, Box<dyn std::error::Error>> {
    let mut router = RunnerRouter::new();
    router.register(Arc::new(EchoRunner))?;
    Ok(router.build(resource).await?)
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
| `catalog`                             | Current runner registrations        | Immutable, cloneable `RunnerCatalog`    |
| `RunnerCatalog::build`                | `Task` and explicit `BuildContext`  | `BuiltTask`                             |
| `RunnerCatalog::build_scoped_with_cancellation` | Nested task, context, cancellation, and `BuildScope` | Admitted nested `BuiltTask` |
| `pick`                                | `Task`                              | First matching `Runner`                 |
| `build`                               | `Task`                              | `BuiltTask`                             |
| `capabilities`                        | Registered entries                  | Owned `AgentCapabilities` snapshot      |
| `merge_env`                           | `TaskEnv` and `RunnerEnv`           | Sorted process environment              |
| `OutputSink::stdout_line`             | Raw `Bytes`, optionally LF-framed   | Delimiter-free `OutputEvent::Chunk`     |
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
    Runner::build_task(Task, RunId, BuildContext, BuildCancellation, BuildScope)
                          │
                          ▼
                 taskvisor::TaskRef
                          │ router pairs it with RunId
                          ▼
                      BuiltTask
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

## Runner composition

`RunnerRouter::catalog()` captures the current registrations, including their labels and routing priority.
The catalog is immutable and cheap to clone.
Later registrations do not change it.

Take the catalog before registering a composing runner such as `ChainRunner`:

```rust,ignore
let inner_runners = router.catalog();
router.register(Arc::new(ChainRunner::new("chain", inner_runners)))?;
```

The composing runner calls `RunnerCatalog::build_scoped_with_cancellation` for
each inner task and passes its inherited `BuildScope`. This reuses the outer
global admission slot and applies the selected inner runner's per-runner limit.
Catalog builds use the same exact GVK and selector routing, registration order, and `RunId` allocation as `RunnerRouter::build`.

Direct `RunnerRouter::build` and `RunnerCatalog::build` calls are unmanaged: no
core admission limits apply. A scoped catalog build returns
`RouterError::RecursiveBuild` before admission when it selects a runner already
present in the active build path. It returns `RouterError::AdmissionCycle` when
its nested admission wait would deadlock with other active root builds.

## Build contract

`RunnerRouter::build` allocates a `RunId`.
Its format is `{runner}-{slot}-{seq}`.
The sequence comes from one process-global counter initialized to `1`.

The router passes the resource, run ID, `BuildContext`, a read-only
`BuildCancellation` signal, and an opaque `BuildScope` to `Runner::build_task`.
It returns a `BuiltTask` that keeps the same run ID beside the executable
`TaskRef`. Use `BuiltTask::name` when constructing the surrounding
`taskvisor::TaskSpec`.

`build_task` constructs a task but does not start it.
It is asynchronous and receives a cancellation signal for obsolete generations,
shutdown, and supervisor-enforced deadlines.
All build work must remain owned by the returned future. Dropping that future must
not leave background work running. A runner that delegates inherently blocking
work must own a bounded facility with explicit cancellation and shutdown behavior.
The returned `TaskRef` must not retain the build cancellation signal.
Submission can still be rejected after construction.
A returned task can run more than once under its Taskvisor restart policy.
Attempt-scoped resources belong inside the task body.

## Migrating async builds

Implementations written for the synchronous runner contract require three changes:

1. Add `#[solti_runner::async_trait]` to the `Runner` implementation.
2. Change `build_task` to `async fn` and accept `&BuildCancellation` and
   `&mut BuildScope`.
3. Await `RunnerRouter::build`; composing runners use
   `RunnerCatalog::build_scoped_with_cancellation`.

Use `cancellation.cancelled()` in waits and pass clones to child futures that are
owned by the build. Do not migrate synchronous blocking calls by placing them in
an unbounded or detached `spawn_blocking` task.

Callers that need to cancel a direct router or catalog build create an owner and
signal with `BuildCancellation::pair()`, retain the `BuildCancellationHandle`,
and pass the signal to `build_with_cancellation`. A runner receives only the
read-only signal and cannot request cancellation itself.

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
- LF and CRLF delimiters split input into chunks and are not included in payloads.
- Every emitted chunk receives the next sequence number for its stream.
- Truncated multi-line input marks only its final emitted chunk as truncated.
- All non-delimiter bytes, including invalid UTF-8 and a lone CR, remain exact.
- Delimiter-free `Bytes` passed to `OutputSink::new` are forwarded without a payload copy.
- Borrowed callbacks let bounded composition layers detach input with one copy.
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

`RunnerError` is returned by a concrete runner:

| Variant               | Cause                                      |
|-----------------------|--------------------------------------------|
| `UnsupportedWorkload` | Runner received an unsupported GVK         |
| `InvalidSpec`         | Workload desired state is invalid          |
| `Internal`            | Runner could not construct the task        |

## Examples

### Internal examples

These examples stay inside the `solti-runner` responsibility.
`solti-model` and Taskvisor appear because their types form the public `Runner` contract.
No higher or sibling Solti crate is used.
Each example starts with a text flow diagram, then explains its inputs, decisions, and result.

Start with a custom runner and application-owned workload:

```bash
cargo run -p solti-runner --example custom_extension
```

| Example                                             | What it shows                                                      |
|-----------------------------------------------------|--------------------------------------------------------------------|
| [custom_extension.rs](examples/custom_extension.rs) | Custom GVK, runner selection, capabilities, and allocated `RunId`. |
| [build_context.rs](examples/build_context.rs)       | Environment merge, metrics port, and attempt-scoped output events. |

`solti-runner` has no optional features.
Both examples use its default feature set.

### Full examples

[`task_subprocess.rs`](https://github.com/soltiHQ/sdk/blob/main/crates/solti/examples/task_subprocess.rs) composes a runner with `solti-exec` and `solti-core`.
The complete catalog lives in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
