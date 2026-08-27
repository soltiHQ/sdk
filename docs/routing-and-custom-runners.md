---
title: Route work and add runners
description: Select an execution backend and implement an application-owned workload without changing the SDK model.
---

# Route work and add runners

A runner converts desired workload data into a reusable Taskvisor task.
It does not submit, schedule, or start that task while building it.
Use a custom runner when an application operation needs its own workload contract or backend.

## Participants

| Component | Role and reason to use it |
|---|---|
| `solti-model` | Validates the Task envelope, workload identity, labels, and selectors. An `ExtensionWorkload` carries application-owned JSON. |
| `solti-runner` | Defines `Runner`, selects a backend, and passes environment, output, metrics, build cancellation, and build admission. It has no optional features. |
| `solti-core` | Commits desired state, builds the current generation, and submits its executable task. Use it when Tasks need reconciliation, queries, and lifecycle state. |
| Taskvisor | Executes attempts and owns runtime admission, timeout, restart, and cancellation after submission. |

Built-in backend implementations live in [solti-exec](subprocesses.md).
[solti-chain](chains.md) implements a composing runner through the same interface.
The model's optional `schema` feature describes wire data; it does not install a runner.

## Understand the integration flow

```text
TaskManifest → core desired-state commit → current Task generation
                                              ↓
                               exact workload GVK + runnerSelector
                                              ↓
                               Runner::build_task → BuiltTask
                                              ↓
                                  Taskvisor submission → attempts
```

`BuiltTask` pairs a `RunId` with a `taskvisor::TaskRef`.
Core uses the allocated run name for the runtime registration.
The model Task name, controller slot, and generated run name are different identities.
See [task resources](task-resources.md) and [lifecycle and admission](lifecycle-and-admission.md).

An accepted desired-state write does not prove that routing or building succeeded.
Read the Task's `Reconciled` condition for that generation.
A successfully built task can still be rejected by runtime admission.

## Select a backend deliberately

`RunnerRouter` applies these rules:

1. Reject built-in `Embedded` workloads. Their `TaskRef` is supplied through core's embedded-task API.
2. Match the exact workload `apiVersion` and `kind`.
3. Apply `TaskSpec::runner_selector` to static runner labels, when configured.
4. Select the first remaining runner in registration order.

Several matches are allowed. The router emits a diagnostic and chooses the first; it does not return an ambiguity error or balance requests.
No match returns `RouterError::NoRunner`.
The router does not inspect application payload fields.

Register labels on the backend, not on the Task resource:

```rust
use std::sync::Arc;
use solti_model::Labels;
use solti_runner::{RouterError, Runner, RunnerRouter};

fn register_gpu(backend: Arc<dyn Runner>) -> Result<RunnerRouter, RouterError> {
    let mut labels = Labels::new();
    labels.insert("accelerator", "gpu");
    let mut router = RunnerRouter::new();
    router.register_with_labels(backend, labels)?;
    Ok(router)
}
```

Then use an object-valued selector in the Task spec:

```yaml
runnerSelector:
  matchLabels:
    accelerator: gpu
```

A Task's metadata labels serve resource selection and do not select its runner.

Registration validates and snapshots the runner name, labels, and declared workload types.
Names must be unique and valid label values; the workload list must be nonempty, contain no duplicates, and exclude built-in `Embedded`.
Changing an implementation's declarations later does not change the registered snapshot.
`capabilities()` returns that routing catalog as an owned capability snapshot.
Capabilities describe registration, not backend health or caller authorization.

## Define a strict extension contract

An extension uses the normal workload envelope:

```json
{
  "apiVersion": "media.example.io/v1",
  "kind": "ImageResize",
  "spec": { "source": "input.png", "width": 320 }
}
```

The model validates the GVK, requires an object-valued `spec`, and bounds JSON nesting.
The `solti.io` API group is reserved for built-in workloads.
Unknown envelope fields are rejected, but extension `spec` fields remain application-owned.
The model does not know which fields your backend accepts.

Validate the payload in `build_task`, before returning an executable task.
The [custom extension example](../crates/solti-runner/examples/custom_extension.rs) uses this pattern:

```rust
use serde::Deserialize;
use solti_model::ExtensionWorkload;
use solti_runner::RunnerError;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageResizeSpec {
    source: String,
    width: u32,
}

fn decode_spec(extension: &ExtensionWorkload) -> Result<ImageResizeSpec, RunnerError> {
    let spec: ImageResizeSpec = serde_json::from_value(extension.spec().clone())
        .map_err(|error| RunnerError::InvalidSpec(error.to_string()))?;
    if spec.source.trim().is_empty() || spec.width == 0 {
        return Err(RunnerError::InvalidSpec("source and positive width are required".into()));
    }
    Ok(spec)
}
```

The enclosing runner must also check the workload variant and exact GVK.
`workload_types()` advertises routing support; it is not a replacement for payload validation.
Use `RunnerError::UnsupportedWorkload` for the wrong workload and `InvalidSpec` for invalid desired data.

## Keep build and attempt ownership separate

Implement `Runner` with `#[solti_runner::async_trait]` and three methods:

- `name()` identifies the backend registration;
- `workload_types()` declares supported GVKs;
- async `build_task(task, run_id, context, cancellation, scope)` returns a `TaskRef`.

Build work must remain owned by the build future.
Observe `BuildCancellation` during interruptible waits and pass it to owned child operations.
Do not detach blocking or asynchronous build work.
An inherently blocking backend needs its own bounded facility and explicit shutdown contract.

The returned task must not retain the build cancellation signal.
Each attempt receives a separate `TaskContext`; use that for runtime cancellation.
Create attempt-scoped processes, connections, and cleanup state inside the returned future.
One `TaskRef` may run more than once under restart policy, including after an earlier attempt failed.
See [cancellation and shutdown](cancellation-and-shutdown.md).

Construction failures are `RunnerError`/`RouterError`, not failed execution attempts.
Runtime work returns `TaskError`: distinguish retryable failure, fatal failure, and cancellation.
The selected runtime policy decides whether a retryable result starts another attempt.
Do not encode retry decisions by matching error text.

## Bound managed builds

Core's `ReconciliationConfig` defaults to a 30-second build deadline, 32 outer builds, and 8 builds per runner.
The deadline begins before root admission and includes nested builds and their admission waits.
It is separate from the Task's attempt timeout and controller queue residence.

Direct `RunnerRouter::build` and `RunnerCatalog::build` are unmanaged.
They do not inherit core's build limits or deadline.
Direct callers can use `BuildCancellation::pair()` and `build_with_cancellation`; the caller retains the owner handle and supplies its own deadline.

A composing runner must pass the received `BuildScope` into `RunnerCatalog::build_scoped_with_cancellation`.
Nested builds reuse the outer global permit and acquire the selected runner's per-runner permit.
Do not replace the scope with an unmanaged one.
Re-entering an active runner returns `RecursiveBuild`; a nested wait that closes an admission deadlock returns `AdmissionCycle`.
Preserve nested errors with `RunnerError::NestedBuild` when composing backends.

See [reconciliation](reconciliation.md) and [configuration](configuration.md) for the application-owned settings.

## Pass environment and output through the context

`BuildContext` defaults to an empty `RunnerEnv`, no-op metrics, and disabled output.
`merge_env` keeps the last value within each input and lets runner values override task values.
Concrete backends decide how that result combines with a host or image environment.

Acquire `OutputPublisher::sink_for` from the attempt future before spawning output-reader tasks.
Clone the returned sink into readers.
Composing runners can use task-local output routing that a separately spawned task does not inherit.
The sink carries the attempt and generation; stdout and stderr have independent sequences.
Publishing is synchronous, and callbacks must not block execution.
Runners publish chunks, not lifecycle markers or retained history.

Core installs its output publisher when it consumes the router.
For direct builds, install your own publisher if output is required.
Installed metrics and output callbacks have sticky panic containment; callback failure is not a durable delivery guarantee.
See [output and history](output-and-history.md) and [observability](observability.md).

## Run an existing example

```bash
cargo run -p solti-runner --example custom_extension
cargo run -p solti-runner --example build_context
```

`custom_extension` registers two backends and selects one by label.
Its task prints the chosen operation; it does not implement image processing.
`build_context` demonstrates environment precedence, metrics, and attempt-scoped output.
For a complete supervised execution path, use [task_subprocess.rs](../crates/solti/examples/task_subprocess.rs).

`WasmSpec` is a model contract, not a bundled WASM executor.
Executing `solti.io/v1/Wasm` requires an implementation registered for that GVK.
Host process controls do not isolate an in-process WASM engine.

## See also

- [Build an agent](building-an-agent.md): own startup, services, and shutdown.
- [Subprocesses](subprocesses.md), [containers and isolation](containers-and-isolation.md), and [chains](chains.md): concrete runner processes.
- Source: [runner contract](../crates/solti-runner/src/runner.rs), [routing](../crates/solti-runner/src/router.rs), [build admission](../crates/solti-runner/src/admission.rs), [build context](../crates/solti-runner/src/context.rs), and [workload validation](../crates/solti-model/src/domain/kind/task.rs).
