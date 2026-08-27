---
title: Quick start
description: Commit one Embedded Task, observe the correct resource incarnation and generation, and join SDK shutdown.
---

# Quick start

This program submits one in-process Task, observes success through a Task watch,
and joins shutdown. It needs no API server, shell, container daemon, or control plane.
An Embedded workload deliberately bypasses runner selection.

## Add the dependencies

Create a Rust binary project and use these dependencies:

```toml
[dependencies]
solti = { version = "0.0.5", default-features = false, features = ["core"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
tokio-stream = "0.1"
```

Rust 1.90 or newer is required. See [installation](installation.md) for direct
component dependencies and optional features.

## Add the program

Put this in `src/main.rs`:

```rust
use std::{io, time::Duration};

use solti::{
    core::SupervisorApi,
    model::{
        AdmissionPolicy, EmbeddedSpec, RestartPolicy, TaskFilter, TaskManifest,
        TaskPhase, TaskSpec, TaskWorkload,
    },
    runner::RunnerRouter,
    taskvisor::{TaskContext, TaskError, TaskFn},
};
use tokio_stream::StreamExt;

type AppResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> AppResult {
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;

    let work_result: AppResult = async {
        let workload = TaskWorkload::Embedded(EmbeddedSpec::new("hello-v1")?);
        let spec = TaskSpec::builder("hello-slot", workload, 5_000_u64)
            .restart(RestartPolicy::Never)
            .admission(AdmissionPolicy::Queue)
            .build()?;
        let manifest = TaskManifest::new("hello", spec)?;
        let work = TaskFn::arc(|_: TaskContext| async {
            println!("hello from a supervised attempt");
            Ok::<(), TaskError>(())
        });

        let mut changes = api.watch_tasks(&TaskFilter::new(), Some("0"))?;
        let committed = api.create_embedded_task(manifest, work).await?;
        println!("committed {}", committed.name());

        let observed = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(change) = changes.next().await {
                let task = change?.into_object();
                if task.metadata().uid() == committed.metadata().uid()
                    && task.metadata().generation() == committed.metadata().generation()
                    && task.status().phase().is_terminal()
                {
                    return Ok::<_, Box<dyn std::error::Error>>(task);
                }
            }
            Err(io::Error::other("watch closed before the expected result").into())
        })
        .await??;

        println!("observed {}: {}", observed.name(), observed.status().phase());
        if observed.status().phase() != TaskPhase::Succeeded {
            return Err(io::Error::other(format!(
                "task did not succeed: {:?}", observed.status()
            )).into());
        }
        Ok(())
    }.await;

    let shutdown_result = api.shutdown().await;
    work_result?;
    shutdown_result?;
    Ok(())
}
```

Run `cargo run`. The program prints the desired-state acknowledgement and the
observed `Succeeded` phase. The task body and the caller run asynchronously;
their print order is not an acknowledgement contract.

## Understand the participants

| Participant | Role in this program |
|---|---|
| `solti-model` | Validates the manifest and defines identity, desired policy, and observed phase. |
| `solti-runner` | Supplies the router required by core; no registered backend is needed for Embedded work. |
| `solti-core` | Commits the resource, reconciles the supplied TaskRef, maintains the watch, and owns SDK shutdown. |
| Taskvisor | Executes the TaskRef under the selected attempt and slot policies. |
| Application | Owns the implementation revision, work, observation deadline, error handling, and shutdown call. |

## Know what the example proves

- The watch opens before the write, avoiding a subscribe-after-completion gap.
- Observation matches UID and generation, not only a reusable resource name.
- `Never` makes this one-attempt example different from a retrying or periodic Task.
- `Queue` is explicit. The SDK default is `DropIfRunning`; the example does not change it.
- The five-second attempt timeout and ten-second observation deadline are example values, not SDK defaults or an end-to-end guarantee.
- The application joins shutdown even when its work/observation block returns an error.

The observed phase does not prove physical slot release or destruction of every
owned value. Core's shutdown result has its own documented boundary.
Read [cancellation and shutdown](cancellation-and-shutdown.md).

## Continue with a real process

- [Task resources](task-resources.md): choose identities, workloads, labels, and guards.
- [Subprocesses](subprocesses.md): register a real execution backend and retain its cleanup handle.
- [Custom runners](routing-and-custom-runners.md): route an application-owned workload kind.
- [Build an agent](building-an-agent.md): add an API, discovery, logging, or metrics without losing ownership boundaries.
- [Existing Embedded lifecycle example](../crates/solti-core/examples/embedded_lifecycle.rs): replace a generation and inspect retained runs.

Source: [core submission and watch API](../crates/solti-core/src/supervisor/mod.rs),
[manifest policy](../crates/solti-model/src/resource/spec.rs), and
[Task watch implementation](../crates/solti-core/src/state/mod.rs).
