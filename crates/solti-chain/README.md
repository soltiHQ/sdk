# solti-chain

Conditional composite workload runner for the Solti task SDK.

`Chain` is one ordinary Solti `Task`. 
Its steps are nested workloads, not child Task resources. 
Exactly one-step runs at a time and selects at most one next step through `onSuccess` or `onFailure`.

The outer Task owns timeout, restart, backoff, admission, cancellation, status, history, and output. 
A retry starts again from `entry`; completed side effects are not rolled back.

The outer Task carries Chain as a regular extension workload. 
Place this JSON at `spec.workload` in a request sent through the existing Task API:

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
        "onFailure": { "next": "cleanup", "mode": "preserve" }
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
        ChainStep::new("deploy", workload())?
            .with_on_success("verify")?,
        ChainStep::new("verify", workload())?
            .with_on_failure("rollback", FailureMode::Preserve)?,
        ChainStep::new("rollback", workload())?,
    ],
)?;
let workload: TaskWorkload = chain.try_into()?;
# let _ = (router, workload);
# Ok(())
# }
```

`FailureMode::Preserve` runs the handler path but returns the first preserved failure if that path otherwise succeeds. 
`FailureMode::Recover` discards the current failure, allowing a successful handler path to complete the chain.

Cancellation always stops the active step and bypasses `onFailure`. 
Outer Taskvisor timeout and panic handling also happen outside the chain future; they do not select an `onFailure` transition.
