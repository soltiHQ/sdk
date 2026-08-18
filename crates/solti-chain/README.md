# solti-chain

`solti-chain` adds a conditional chain workload to the Solti task SDK.

A chain is one Solti `Task` with several nested workload steps.
Only one-step runs at a time.
Each step can select one next step after success and one after failure.

Taskvisor owns timeout, restart, backoff, admission, cancellation, status, history, and output for the whole chain.
The crate does not provide parallel DAG execution or separate Task resources for steps.

## Quick start

Register leaf runners first, then register the chain runner:

```rust,no_run
use solti_chain::{ChainSpec, ChainStep, FailureMode, register_chain_runner};
use solti_model::TaskWorkload;
use solti_runner::RunnerRouter;

# fn workload() -> TaskWorkload { unimplemented!() }
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut router = RunnerRouter::new();
// Register Subprocess, Container, and custom leaf runners first.

register_chain_runner(&mut router, "chain")?;

let chain = ChainSpec::new(
    "deploy",
    vec![
        ChainStep::new("deploy", workload())?.with_on_success("verify")?,
        ChainStep::new("verify", workload())?
            .with_on_failure("rollback", FailureMode::Preserve)?,
        ChainStep::new("rollback", workload())?,
    ],
)?;
let workload = chain.into_workload()?;
# let _: TaskWorkload = workload;
# let _ = router;
# Ok(())
# }
```

The runner catalog is captured when `register_chain_runner` is called.
Runners registered later are not available to chain steps.

## What it does

- provides a typed model for the `chain.solti.io/v1alpha1/Chain` extension workload;
- routes every nested workload through a snapshot of registered leaf runners;
- validates step names, transitions, reachability, and cycles;
- sends all step output through the outer Task output stream;
- supports success and failure transitions;
- runs one selected step at a time.

## Inputs and outputs

| API                        | Input                                 | Output                         |
|----------------------------|---------------------------------------|--------------------------------|
| `ChainStep::new`           | Step name and `TaskWorkload`          | Validated step                 |
| `with_on_success`          | Next step name                        | Success transition             |
| `with_on_failure`          | Next step name and `FailureMode`      | Failure transition             |
| `with_runner_selector`     | `LabelSelector`                       | Step-specific runner selection |
| `ChainSpec::new`           | Entry name and steps                  | Validated acyclic chain        |
| `ChainSpec::into_workload` | `ChainSpec`                           | Solti extension `TaskWorkload` |
| `ChainSpec::from_workload` | Solti extension `TaskWorkload`        | Validated `ChainSpec`          |
| `register_chain_runner`    | `RunnerRouter` and runner name        | Registered `ChainRunner`       |
| `ChainRunner::build_task`  | Standard runner build inputs          | One outer `taskvisor::TaskRef` |

The runner build inputs are the outer `Task`, allocated `RunId`, explicit
`BuildContext`, read-only `BuildCancellation` signal, and inherited
`BuildScope`. Chain uses scoped catalog builds. Core admission counts every
leaf runner without reacquiring the outer global slot.

## Workload contract

| Field        | Value                       |
|--------------|-----------------------------|
| `apiVersion` | `chain.solti.io/v1alpha1`   |
| `kind`       | `Chain`                     |

The outer Task carries the chain as a regular extension workload.
For HTTP, place this object at `Task.spec.workload`:

```json
{
  "apiVersion": "chain.solti.io/v1alpha1",
  "kind": "Chain",
  "spec": {
    "entry": "build",
    "steps": [
      {
        "name": "build",
        "workload": {
          "apiVersion": "jobs.example.io/v1",
          "kind": "Build",
          "spec": { "target": "app" }
        },
        "onSuccess": "publish"
      },
      {
        "name": "publish",
        "workload": {
          "apiVersion": "jobs.example.io/v1",
          "kind": "Publish",
          "spec": { "target": "app" }
        },
        "onFailure": {
          "next": "cleanup",
          "mode": "preserve"
        }
      },
      {
        "name": "cleanup",
        "workload": {
          "apiVersion": "jobs.example.io/v1",
          "kind": "Cleanup",
          "spec": { "target": "app" }
        }
      }
    ]
  }
}
```

For gRPC, use `TaskWorkload` with the same `api_version` and `kind`.
Select the `extension` field and put the UTF-8 JSON bytes of the inner chain `spec` object in `ExtensionTask.spec.raw`.
Protobuf JSON represents these bytes as base64.

HTTP and gRPC carry Chain through the existing generic extension boundary.
The chain graph is validated when `ChainRunner` builds the Task during reconciliation.

## Execution flow

```text
outer Task
    ▼
  entry
    ├── success ──► onSuccess
    └── failure ──► onFailure
                         ▼
                    next step
```

There is one current step and at most one selected next step.
A missing `onSuccess` ends the selected path successfully.
A missing `onFailure` returns the step error.

This model is an outcome-directed acyclic chain.
It is not a parallel DAG scheduler.

## Validation

`ChainSpec::new`, deserialization, and `ChainSpec::validate` apply these rules:

- the chain contains at least one step;
- `entry`, step names, and transition targets use `TaskId` validation;
- step names are unique;
- `entry` and every transition name a declared step;
- every step is reachable from `entry`;
- the transition graph has no cycles;
- every `runnerSelector` is valid;
- `Embedded` workloads are rejected;
- nested `chain.solti.io/v1alpha1/Chain` workloads are rejected.

`ChainRunner` builds every declared step before the chain starts.
If any step cannot be routed or built, reconciliation fails even when that step would not be selected in the current run.

## Failure transitions

| Mode       | Behavior                                                                 |
|------------|--------------------------------------------------------------------------|
| `Preserve` | Keeps the first selected failure while the handler path runs.            |
| `Recover`  | Does not preserve the selected failure. A successful path may recover.   |

`Preserve` is the default wire value when `mode` is not present.
If its handler path otherwise ends successfully, the preserved failure becomes the chain result.

Cancellation stops the active step and does not select `onFailure`.
An outer Taskvisor timeout or panic is handled outside the chain future and also does not select `onFailure`.

## Runner composition

`register_chain_runner` takes an immutable snapshot of the current `RunnerRouter` catalog, then registers `ChainRunner`:

```text
leaf runners registered
          ▼
 RunnerRouter::catalog
          │ immutable snapshot
          ▼
      ChainRunner
          ▼
 build each step through RunnerCatalog
```

Register every allowed leaf runner before the chain runner.
The snapshot prevents a chain from routing another chain through the same registration.

A step uses its own `runnerSelector`.
The outer Chain Task selector is not copied to nested workloads.

## Task ownership

| Outer Task owns                         | Steps do not own                       |
|-----------------------------------------|----------------------------------------|
| timeout and cancellation                | independent timeout or cancellation    |
| restart and backoff                     | independent restart or backoff         |
| admission and concurrency slot          | independent admission slot             |
| status and retained run history         | separate Task status or run history    |
| generation, attempt, and live output    | separate API identity                  |

A retry starts the chain again from `entry`.
Completed side effects are not rolled back.

## Output

All steps publish to one outer Task output sink for the current attempt.
Concurrent attempts of the same built chain keep separate sinks and sequence counters.
Nested runners must acquire their sink from the attempt future before spawning
separate output tasks, as required by the `solti-runner` output contract.
The chain also writes step markers:

```text
[chain] step=deploy state=started
[chain] step=deploy state=succeeded
[chain] step=verify state=failed
```

Started and successful markers use stdout.
Failed and canceled markers use stderr.
Step output and markers share the outer generation, attempt, and per-stream sequence.

The API output model has no separate step field.
The marker text provides step context in the combined stream.

## Features

| Feature  | Default | Effect                                                |
|----------|---------|-------------------------------------------------------|
| `schema` | yes     | Implements JSON Schema support for Chain model types. |

Disable default features when schema generation is not needed.

## Errors

`ChainError` describes model construction, validation, and conversion failures:

| Variant              | Cause                                                   |
|----------------------|---------------------------------------------------------|
| `Invalid`            | A field, workload, transition, or graph rule is invalid |
| `UnexpectedWorkload` | A workload does not use the Chain extension GVK         |
| `Json`               | The extension `spec` cannot be encoded or decoded       |
| `Model`              | The shared Solti model rejects a nested value           |

Runner selection and build failures use `solti_runner::RunnerError` and `RouterError`.

## Examples

### Internal examples

The focused model and runtime suites stay inside the `solti-chain` responsibility:

```bash
cargo test -p solti-chain --all-features
```

| Source                         | What it shows                                           |
|--------------------------------|---------------------------------------------------------|
| [model.rs](tests/model.rs)     | Wire shape, validation, schema, and conversion          |
| [runtime.rs](tests/runtime.rs) | Routing, transitions, cancellation, retry, and output   |

### Full examples

[`task_chain.rs`](https://github.com/soltiHQ/sdk/blob/main/crates/solti/examples/task_chain.rs) runs four subprocess steps through `solti-core`.
The third step fails, the fourth step recovers the chain, and the outer Task succeeds.

Run it from the SDK workspace:

```bash
cargo run -p solti --example task_chain --features chain,core,exec-subprocess
```

The complete catalog lives in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
