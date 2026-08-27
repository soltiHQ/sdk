---
title: Run conditional chains
description: Compose routed workloads into one supervised Task with explicit success and failure paths.
---

# Run conditional chains

A chain is one Task whose attempt follows a path through several workload steps.
Only one step runs at a time.
Use it when a later operation depends on the previous operation's result, such as deploy, verify, and rollback.
It is not a parallel DAG scheduler, transaction engine, or durable workflow store.

## Participants

| Component | Role and reason to use it |
|---|---|
| `solti-chain` | Defines and validates the graph, builds its steps, follows transitions, and combines output. Its default `schema` feature adds JSON Schema support. |
| `solti-model` | Carries the chain as an `ExtensionWorkload` inside the outer Task. |
| `solti-runner` | Selects leaf backends through a captured catalog and carries shared build admission and cancellation. |
| Leaf runners | Implement each step's actual work and resource cleanup. They may come from `solti-exec` or the application. |
| `solti-core` and Taskvisor | Reconcile, admit, supervise, and report the outer Task. Steps are not independently submitted Tasks. |

With the facade crate, enable `solti/chain` and the features required by the leaf backends.
The complete subprocess example also needs `core` and `exec-subprocess`.
Use `chain-schema` for the facade's JSON Schema support.
Schema support describes the model; runtime graph validation still applies.

## Register leaf runners first

```rust
use solti_chain::register_chain_runner;
use solti_exec::subprocess::register_subprocess_runner;
use solti_runner::RunnerRouter;

fn configure() -> Result<
    (RunnerRouter, std::sync::Arc<solti_exec::subprocess::SubprocessRunner>),
    Box<dyn std::error::Error>,
> {
    let mut router = RunnerRouter::new();
    let subprocess = register_subprocess_runner(&mut router, "subprocess")?;
    register_chain_runner(&mut router, "chain")?;
    Ok((router, subprocess))
}
```

`register_chain_runner` snapshots the current `RunnerCatalog` before registering itself.
Runners added later can serve top-level Tasks but are absent from this chain's catalog.
Keep the subprocess handle from this example for its final shutdown.

A step uses its own `runnerSelector`.
The outer Task selector chooses the chain runner and is not copied to leaf workloads.
Leaf selection still uses exact GVK, labels, and first matching registration.
See [routing and custom runners](routing-and-custom-runners.md).

## Define a graph

The chain workload identity is `chain.solti.io/v1alpha1`, kind `Chain`.
The following function composes three caller-provided workloads without starting them:

```rust
use solti_chain::{ChainError, ChainSpec, ChainStep, FailureMode};
use solti_model::TaskWorkload;

fn deployment(
    deploy: TaskWorkload,
    verify: TaskWorkload,
    rollback: TaskWorkload,
) -> Result<TaskWorkload, ChainError> {
    ChainSpec::new(
        "deploy",
        vec![
            ChainStep::new("deploy", deploy)?.with_on_success("verify")?,
            ChainStep::new("verify", verify)?
                .with_on_failure("rollback", FailureMode::Preserve)?,
            ChainStep::new("rollback", rollback)?,
        ],
    )?
    .into_workload()
}
```

Put the returned workload in the outer `TaskSpec` and submit a `TaskManifest` through core.
Choose timeout, admission, and restart policy on that outer spec.
The example graph returns failure after a successful rollback because the verify failure is preserved.
Use `Recover` only when a successful handler path should allow the outer Task to succeed.

Constructors and deserialization reject:

- an empty graph, invalid names, or duplicate step names;
- a missing entry or transition target;
- any declared step unreachable from the entry;
- a cycle across either success or failure edges;
- invalid nested workloads or selectors;
- built-in `Embedded` steps and nested Chain workloads with this chain GVK;
- unknown chain, step, or failure-transition fields.

Step names use `TaskId` validation.
The step's workload is data for a runner, not an independently configured `TaskSpec`.
There are no per-step timeout, restart, or admission fields.

## Separate graph build from execution

```text
outer reconciliation
    → validate graph
    → build every declared leaf in manifest order
    → return one reusable chain TaskRef

each outer attempt
    → entry → selected success/failure edges → one final result
```

All declared steps must route and build successfully before any step starts.
This includes a failure handler that the current attempt may never select.
A bad unused branch therefore fails reconciliation, not a later conditional execution.

Each leaf build receives the inherited `BuildScope` and build cancellation signal.
Core-managed nested builds reuse the outer global build permit and count against the leaf runner's limit.
The outer build deadline includes these nested builds and their admission waits.
Original nested routing and build errors remain available through `RunnerError::NestedBuild`.

The chain retains the built leaf `TaskRef` values.
At runtime it calls each selected leaf with a child `TaskContext` inside the outer attempt.
Leaf attempt resources and cleanup belong to that leaf's backend.
Building a chain does not itself reserve a controller slot for every step.

## Know how a result chooses the next step

| Step result | Transition and final result |
|---|---|
| Success with `onSuccess` | Run that target. |
| Success without `onSuccess` | Finish with success unless an earlier failure was preserved. |
| Non-cancellation error without `onFailure` | Finish with this step's error. |
| Error with `onFailure: Preserve` | Keep the first preserved error and run the handler target. A successful end returns the preserved error. |
| Error with `onFailure: Recover` | Run the handler target without preserving this error. A successful end can succeed. |
| `TaskError::Canceled` | Stop immediately; do not select a failure handler. |

`Preserve` is the wire default when `mode` is omitted.
A later `Recover` does not clear an error already stored by an earlier `Preserve`.
If a later step fails without its own failure transition, that later error is returned.
Failure transitions can handle both retryable and fatal task errors; they do not handle cancellation.

Cancellation of the outer context stops the active step.
An outer timeout or task panic is handled outside the chain future and does not enter `onFailure`.
A rollback edge is therefore not an unconditional finalizer.
Backend resource cleanup must work when the active future is dropped.
See [cancellation and shutdown](cancellation-and-shutdown.md).

## Retry the whole attempt

The outer Task owns admission, timeout, restart, backoff, phase, and run history.
A restart begins at `entry` again with no saved graph cursor.
Steps that succeeded in the previous attempt can execute again.
The chain does not undo external side effects or persist checkpoints.
Use operations that tolerate repetition or application-owned idempotency when retries are enabled.

A successful `Recover` path changes the outer result to success only when no earlier failure remains preserved.
That result then follows the outer restart policy; the handler does not install a new policy.
See [lifecycle and admission](lifecycle-and-admission.md) and [production boundaries](production-boundaries.md).

## Read one combined output stream

All step output uses one outer Task sink for the current attempt.
Steps and chain markers share the outer generation, attempt number, and per-stream sequence counters.
Concurrent attempts of the same built chain use separate sinks.

```text
[chain] step=deploy state=started
[chain] step=deploy state=succeeded
[chain] step=verify state=failed
```

Started and succeeded markers use stdout; failed and canceled markers use stderr.
The output model has no separate step field.
Markers supply context in the combined stream, not separate step status or run history.
No sink means no published markers.

A custom leaf must acquire its output sink while its attempt future is being polled, then clone it into any spawned readers.
The chain's output adapter is task-local and is not automatically inherited by a new Tokio task.
See [output and history](output-and-history.md).

## Run the complete example

```bash
cargo run -p solti --example task_chain --features chain,core,exec-subprocess
```

[task_chain.rs](../crates/solti/examples/task_chain.rs) starts the example executable as four real subprocess steps.
The third fails, the fourth recovers, and the program checks one successful outer run.
It uses local TCP readiness gates to subscribe before the children write output; it does not require a shell or external service.
It shuts down core and then the subprocess finalizer.

The [model tests](../crates/solti-chain/tests/model.rs) cover graph validation and wire data.
The [runtime tests](../crates/solti-chain/tests/runtime.rs) provide focused examples of transitions, cancellation, retry, and shared output.

## See also

- [Routing and custom runners](routing-and-custom-runners.md): catalog snapshots and nested build admission.
- [Subprocesses](subprocesses.md) and [containers and isolation](containers-and-isolation.md): leaf attempt ownership.
- Source: [chain model](../crates/solti-chain/src/model.rs), [runner and transitions](../crates/solti-chain/src/runner.rs), and [attempt-local output](../crates/solti-chain/src/output.rs).
