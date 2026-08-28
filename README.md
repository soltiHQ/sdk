# Solti SDK

[![Crates.io](https://img.shields.io/crates/v/solti.svg)](https://crates.io/crates/solti)
[![docs.rs](https://docs.rs/solti/badge.svg)](https://docs.rs/solti)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> Build task-execution agents from Kubernetes-shaped resources, pluggable runners, and optional HTTP or gRPC boundaries.

Solti is a modular Rust SDK.
It provides the resource model, routing, reconciliation, execution backends, APIs, discovery, TLS, logging, and metrics used by an agent binary.

Your binary selects the required crates or enables them through the `solti` umbrella crate.
Your binary still owns configuration, deployment, and the final security boundary.

Solti uses [Taskvisor](https://github.com/soltiHQ/taskvisor) for supervised attempt lifecycles.

| [Documentation](docs/index.md) | [Quick start](#quick-start) | [Architecture](#architecture) | [Platform limits](#execution-backends-and-platform-limits) | [Examples](#examples) | [Benchmarks](#benchmarks) |

## Documentation

The [SDK guides](docs/index.md) follow processes across crates: building an
agent, managing desired work, executing workloads, exposing APIs, and operating
the runtime. Each guide identifies the participants, ownership, and guarantees.

Start with the [mental model](docs/mental-model.md), use the
[architecture map](docs/architecture.md) to locate a responsibility, or choose
a runnable program from the [complete example catalog](docs/example-catalog.md).
The [API reference map](docs/api-reference.md) links all product crates.

## The stack you stop wiring

A task agent needs more than process spawning.
It must validate desired state, select a runtime, reconcile changes, supervise attempts, expose status, and shut down cleanly.

Solti separates those responsibilities:

| Concern            | SDK boundary                                                   |
|--------------------|----------------------------------------------------------------|
| Resource contract  | Kubernetes-shaped `Task`, metadata, spec, status, and watches  |
| Workload selection | GVK routing plus optional runner label selectors               |
| Desired state      | Asynchronous latest-wins reconciliation                        |
| Attempt lifecycle  | Taskvisor restart, timeout, cancellation, and admission        |
| Execution          | Subprocess, native containerd 2.x, or application-owned runner |
| Public API         | HTTP/JSON or gRPC                                              |
| Agent registration | Outbound HTTP or gRPC discovery                                |
| Operations         | TLS, logging, Prometheus, and live output                      |

Each layer has a direct crate.
The umbrella crate only forwards features and namespaces.

## Check the fit first

Use Solti when a binary needs versioned Task resources, pluggable workload kinds, desired-state reconciliation, or a public agent API.

If the process only needs to supervise ordinary async functions, use [Taskvisor](https://github.com/soltiHQ/taskvisor) directly.
It has a smaller API and no resource or network layer.

Solti is not a durable job system or a control plane.
Core state and live output are process-local.
A compatible control plane, persistent storage, authorization policy, and deployment topology remain separate concerns.

## Quick start

Add the umbrella crate with only the required capabilities:

```toml
[dependencies]
solti = { version = "0.0", features = ["core", "exec-subprocess"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

Put this in `src/main.rs`, then run `cargo run`.
The parent registers a subprocess runner and supervises one execution of the same binary.

```rust,no_run
use std::{env, io, time::Duration};

use solti::{
    core::SupervisorApi,
    exec::subprocess::register_subprocess_runner,
    model::{
        Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest, TaskSpec,
        TaskWorkload,
    },
    runner::RunnerRouter,
};

const CHILD_MODE: &str = "--solti-quick-start-child";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        println!("hello from the supervised subprocess");
        return Ok(());
    }

    let mut router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
    let supervisor = SupervisorApi::builder(router).start().await?;

    let command = env::current_exe()?.to_string_lossy().into_owned();
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command,
            args: vec![CHILD_MODE.into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("quick-start", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let committed = supervisor
        .create_task(TaskManifest::new("quick-start", spec)?)
        .await?;
    let name = committed.name().clone();
    println!("committed {name}");

    let phase = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let task = supervisor
                .get_task(&name)
                .ok_or_else(|| io::Error::other("task disappeared"))?;
            let phase = task.status().phase();
            if phase.is_terminal() {
                break Ok::<_, io::Error>(phase);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;

    println!("finished {name}: {phase}");
    supervisor.shutdown().await?;
    subprocess_runner.shutdown(Duration::from_secs(5)).await?;
    Ok(())
}
```

`create_task` returns after desired state is committed.
Reconciliation and execution continue asynchronously.

The complete lifecycle with live output and history is in [task_subprocess.rs](crates/solti/examples/task_subprocess.rs).

## Build only what you need

All `solti` features are disabled by default.
Higher-level features enable their required lower layers.

| Binary requirement                 | Start with                                                              |
|------------------------------------|-------------------------------------------------------------------------|
| Resource types                     | `model`                                                                 |
| Resource types and JSON Schema     | `model-schema`                                                          |
| Custom runner registration         | `runner`                                                                |
| Conditional sequential workloads   | `chain`                                                                 |
| In-process desired-state runtime   | `core`                                                                  |
| Subprocess task runtime            | `core`, `exec-subprocess`                                               |
| Native containerd task runtime     | `core`, `exec-containerd`                                               |
| HTTP Task API                      | `api-core-adapter`, `api-http`, `exec-subprocess`                       |
| gRPC Task API                      | `api-core-adapter`, `api-grpc`, `exec-subprocess`                       |
| gRPC Task API with TLS or mTLS     | `api-core-adapter`, `api-grpc-tls`, `exec-subprocess`                   |
| HTTP agent with discovery          | `api-core-adapter`, `api-http`, `discover-http`, `exec-subprocess`      |
| Complete standard integration set  | `full`                                                                  |

`exec-containerd` is a native containerd 2.x adapter.
It is not a Docker, CRI, or Docker Compose integration.

## Architecture

The component graph is acyclic.
Arrows in the diagram point toward the dependency or runtime contract being consumed.

<p align="center">
  <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/schema/solti-sdk-components.png" alt="Solti SDK component boundaries: the solti umbrella crate selects optional API, discovery, operations, execution, and runtime crates while core and execution meet through the runner contract above the shared model and Taskvisor" width="968">
</p>

`solti-core` never depends on `solti-exec`.
Execution backends implement the `solti-runner` contract and are registered by the binary.

`solti-api` depends on core only through `core-adapter`.
Its handler and transport boundaries can be used with another implementation.

`solti-discover` is an outbound client.
It does not run the agent API or depend on core.

`solti-tls` has no SDK dependencies.
`solti-prometheus` connects to producers only through feature-selected adapters.

## Crates

| Crate                                         | Owns                                                                |
|-----------------------------------------------|---------------------------------------------------------------------|
| [`solti`](crates/solti)                       | Umbrella feature forwarding and canonical namespaces                |
| [`solti-model`](crates/solti-model)           | Resources, workloads, policies, selectors, capabilities, and tokens |
| [`solti-runner`](crates/solti-runner)         | Runner contract, GVK routing, selectors, and execution context      |
| [`solti-chain`](crates/solti-chain)           | Conditional sequential composition of nested workloads              |
| [`solti-core`](crates/solti-core)             | Desired state, reconciliation, watches, history, and live output    |
| [`solti-exec`](crates/solti-exec)             | Execution backends and host-process controls                        |
| [`solti-api`](crates/solti-api)               | HTTP/JSON and gRPC Task APIs                                        |
| [`solti-discover`](crates/solti-discover)     | Agent registration and heartbeat client                             |
| [`solti-tls`](crates/solti-tls)               | TLS and mTLS identities, trust roots, and rustls configuration      |
| [`solti-observe`](crates/solti-observe)       | Structured logging and supervised timezone refresh                  |
| [`solti-prometheus`](crates/solti-prometheus) | Metrics adapters, collectors, and exporter endpoint                 |

Depend on one component crate when one boundary is enough.
Use `solti` when a binary composes several components.

## Task resources

Solti follows Kubernetes resource conventions.
A `Task` has `apiVersion`, `kind`, `metadata`, `spec`, and `status`.

The caller owns desired fields.
Core owns UID, resource version, generation, timestamps, status, and conditions.

Built-in workload kinds are `Subprocess`, `Container`, `Wasm`, and `Embedded`.
`Wasm` is a model contract; this repository does not provide a built-in WASM runner.

Application-owned workload kinds use `ExtensionWorkload` with a non-`solti.io` GVK and strict runner-side decoding.
Runner routing uses workload GVK and an optional Kubernetes-style label selector.

`Embedded` carries an in-process `TaskRef` supplied by the binary.
It bypasses runner routing and has no HTTP or gRPC representation.

### Conditional chains

The optional `solti-chain` runner represents a Chain as one ordinary Task.
Its steps are nested workloads, and exactly one step is active at a time.
Each successful or failed step may select one next step.

Chain uses a regular extension workload under `Task.spec.workload`.
Existing HTTP and gRPC Task operations carry it without a new resource API.

The outer Task owns timeout, restart, backoff, admission, cancellation, status, history, and output.
Steps are not child Task resources and do not have independent lifecycle policies.
Restarting the outer Task starts the chain again from its entry step.

## Reconciliation and execution

Create and apply commit desired state before runtime work begins.
The resource status reports the observed result through phase, generation, attempt, and the `Reconciled` condition.

<p align="center">
  <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/schema/solti-sdk-reconciliation.png" alt="Solti reconciliation: a Task manifest is committed to in-memory state, the latest generation is routed by workload GVK or supplied as an embedded TaskRef, Taskvisor supervises attempts, and status, history, watches, and live output expose the observed result" width="760">
</p>

Reconciliation is latest-wins.
An older generation is discarded before expensive runner construction when a newer generation is already committed.

There is no staged rollout or availability guarantee.
In-flight side effects may finish before the next reconciliation compensates for them.

Core does not run an infinite reconciliation retry queue.
Applying an identical manifest schedules one manual retry only when `Reconciled=False`.

Taskvisor owns attempt restart, backoff, timeout, admission, and cancellation.
Core retains bounded `TaskRun` history separately from the current Task status.
Current retained run values have a separate 256 MiB compact JSON budget by
default. This logical payload bound does not measure allocator overhead or RSS.

## Public Task API

`solti-api` exposes the same task operations over HTTP/JSON and gRPC:

| Operation   | Result                                               |
|-------------|------------------------------------------------------|
| Create      | Commit a new named Task                              |
| Apply       | Create or update desired state                       |
| Get         | Read one Task                                        |
| List        | Filter and paginate a stable collection snapshot     |
| Watch       | Stream retained changes and then live changes        |
| Run history | Paginate a stable retained-attempt snapshot          |
| Cancel      | Reach a terminal logical outcome while retaining desired state and history |
| Logs        | Stream live stdout and stderr for an exact Task UID  |
| Delete      | Reach a terminal logical outcome, then remove the Task and its history |

HTTP uses the fixed root `/apis/solti.io/v1` in the current `solti-api` release.
`HttpApi::build` returns the router and its generated OpenAPI 3.1 document.

Opening a live-output stream requires the current Task UID. Every returned event
repeats that UID, and the stream remains pinned to the same Task incarnation and
generation even if the name is deleted and recreated.

gRPC uses the `solti.task.v1` protobuf package.
Generated server and client types are available from the crate.

Each SDK binary serves the API version compiled into its selected `solti-api` version.
A control plane that manages different binary generations must route each advertised version to its matching contract.

Bearer authentication is disabled until the binary calls `with_auth` or `with_authenticator`.
An optional `with_authorizer` policy runs before each validated Task API operation.
The HTTP and gRPC boundaries enforce a 4 MiB request or message limit.
Task and TaskRun list responses also have 4 MiB native-encoding limits.

Read the complete route, message, pagination, watch, and error contract in [`solti-api/CONTRACT.md`](crates/solti-api/CONTRACT.md).

## Discovery

`solti-discover` advertises one agent endpoint, API version, runner capabilities, identity, and uptime to a control plane.

The discovery loop is returned as an `Embedded` manifest and `TaskRef`.
The binary decides whether to submit it to core.

HTTP and gRPC transports are independent features.
Retryable transport failures use the generated task's Taskvisor policy.
Permanent configuration or authentication failures stop the task.

Discovery does not expose an inbound server.
It does not persist registration state.

Read the versioned wire contract in [`solti-discover/CONTRACT.md`](crates/solti-discover/CONTRACT.md).

## Authentication and TLS

`solti-model::Token` is a redacted bearer secret with constant-time comparison.
`solti-api` can verify it on every route and RPC.
`solti-discover` can send it with every heartbeat.
Discovery bearer tokens require HTTPS by default.
An explicit `allow_insecure_token_transport()` escape hatch exists for controlled
development or loopback endpoints.

The static token is authentication only.
`solti-api` also exposes application hooks for bearer authentication and operation-level authorization.
The SDK does not provide users, tenants, RBAC rules, tenant filtering, policy storage, secret rotation, or secret persistence.

`solti-tls` separates server identity, client identity, and trust roots.
It supports TLS and mandatory client-certificate authentication.

`api-grpc-tls` converts the shared server configuration for tonic.
HTTP server TLS is owned by the server that hosts the axum router.
`discover-tls` applies custom roots or mTLS to outbound HTTPS connections.

The Task API configures bearer authentication and server TLS independently.

## Execution backends and platform limits

> Native containerd execution is Linux-only. macOS can build the adapter and inspect its configuration, but it cannot start a native container attempt.

| Runtime path             | Current platform contract                                 | Isolation boundary                            |
|--------------------------|-----------------------------------------------------------|-----------------------------------------------|
| `Embedded`               | Application's supported Tokio targets                     | Same process; no operating-system isolation   |
| `Subprocess`             | Linux and macOS                                           | Child process; Unix session and process group |
| Non-Unix subprocess path | Implementation exists; Windows is not currently supported | Child process only                            |
| Custom container engine  | Defined by the application adapter                        | Defined by that engine                        |
| Native containerd 2.x    | Linux host and Linux container images                     | OCI runtime plus containerd task lifecycle    |

### Host-process controls

The subprocess path always validates configuration before runner registration.
Configured controls fail closed when the current platform cannot enforce them.

| Control                                           | Platform |
|---------------------------------------------------|----------|
| Session, process group, signal reset, umask       | Unix     |
| `RLIMIT_NOFILE`, `RLIMIT_FSIZE`, core dumps       | Unix     |
| Pinned working directory                          | Unix     |
| Explicit descriptor passlist with `close_range`   | Linux    |
| Atomic `posix_spawn` descriptor passlist          | macOS    |
| Parent snapshot plus child descriptor-table sweep | macOS fallback; other Unix |
| cgroup v2 CPU, memory, and process limits         | Linux    |
| Mount, network, IPC, UTS, and cgroup namespaces   | Linux    |
| UID, GID, supplementary groups, capabilities      | Linux    |
| `no_new_privs` and seccomp denylist               | Linux    |

The default subprocess backend does not enable optional resource or security controls.
It clears the inherited environment, pins the working directory on Unix, restricts descriptor inheritance, and owns child cleanup.
On Linux, a configured subprocess user ID that differs from both the real and
effective agent user IDs requires the agent process to retain effective
`CAP_KILL` so process-group cleanup remains enforceable. A subprocess policy
that retains child `CAP_SETUID` requires the same parent authority.
All runtime threads that can poll those attempts must retain that authority.

These controls harden a host process.
They do not form a complete sandbox for untrusted code.

### Native containerd 2.x

The built-in adapter connects to one explicit Unix socket and namespace.
It does not start or discover containerd.

It requires:

- a Linux host;
- containerd major version 2;
- configured snapshotter and OCI runtime plugins;
- a cached or reachable Linux image;
- an I/O root visible at the same path to the SDK process and containerd;
- `protoc` during builds that compile `containerd-client` bindings.

Network mode is either `none` or `host`.
`none` creates an OCI network namespace without configuring interfaces.
`host` shares the host network namespace.

The adapter does not provide bridge networking, CNI, CRI, port publishing, volumes, or Docker Compose semantics.
The final binary must add those layers if its product requires them.

Run [task_containerd.rs](crates/solti/examples/task_containerd.rs) on a prepared Linux host.
On other platforms the example prints the prerequisite and exits without contacting a daemon.

## State, output, and production limits

Keep these boundaries explicit:

- Core stores Tasks, runs, watch history, and runtime bindings in memory.
- Core retains at most 1024 current Task resources by default.
- Core also retains at most 256 MiB of aggregate TaskManifest bytes by default.
- Current TaskRun values have a separate 256 MiB aggregate compact JSON budget.
- Every current Task counts, including embedded, pending, running, and terminal Tasks.
- Task count, TaskManifest bytes, and current TaskRun bytes are independent limits.
- The TaskManifest budget measures compact canonical TaskManifest JSON. The
  TaskRun budget counts each value currently present in query state once.
- At the count limit, Core rejects new names. At the byte limit, it also rejects
  existing applies that would increase retained bytes past the budget.
- Shrinking and no-op applies remain allowed. Admission never evicts a Task.
- Current-run overflow compacts the globally oldest completed runs. When active
  values alone exceed a smaller custom budget, lifecycle processing and state
  persistence continue while the new active value is omitted from query state.
- `StateConfig` can configure or disable each retained-state limit before startup.
- Serialized byte budgets are logical payload bounds, not allocator or RSS limits.
- Process restart loses all core state.
- Live output is bounded and lossy.
- Core does not persist or replay output by itself.
- Optional core hooks can forward task, run, and output events to an
  application-owned store.
- Lossless state-hook admission has independent event-count and payload-byte
  bounds. The payload bound defaults to 256 MiB and applies backpressure before
  the authoritative state or spawn critical section.
- A slow output subscriber receives `Lagged` after events are dropped.
- Watch history is bounded by change count and serialized Task bytes.
- A watch can resume only while its resource version remains retained.
- Snapshot pagination is consistent only while its continuation remains valid.
- Task and TaskRun list pages are bounded by count and a 4 MiB native response limit.
- Reconciliation is latest-wins and has no staged availability guarantee.
- Discovery registration state is not persisted.
- Static bearer authentication alone does not provide authorization or tenant isolation.
- Host-process controls are hardening, not a complete untrusted-code sandbox.
- The native container adapter provides no CRI or CNI implementation.
- The SDK contains no durable log sink.
- The SDK contains no control-plane server.

Use the persistence hooks with an external store when tasks or logs must
survive process termination. The application owns delivery retries and its
restart recovery flow. The SDK does not load persisted state at startup.
Install application authorization and sandbox policy at the binary or service boundary.

## Feature flags

All umbrella features are off by default.

| Feature or family                | Adds                                                                  |
|----------------------------------|-----------------------------------------------------------------------|
| `model`                          | Runtime model types without JSON Schema support                      |
| `model-schema`                   | Model types with JSON Schema support                                 |
| `runner`                         | Runner contract, model, and Taskvisor                                 |
| `core`                           | Desired-state supervisor and Taskvisor controller                     |
| `exec`                           | Base `solti-exec` namespace                                           |
| `exec-host-process`              | Low-level host-process policy                                         |
| `exec-subprocess`                | Subprocess runner and required lower layers                           |
| `exec-container`                 | Engine-neutral container runner                                       |
| `exec-containerd`                | Native containerd 2.x adapter                                         |
| `exec-seccomp`                   | Linux host-process seccomp renderer                                   |
| `api`                            | API handler and model boundary                                        |
| `api-http`, `api-grpc`           | HTTP or gRPC transport                                                |
| `api-core-adapter`               | Adapter from the API handler to `SupervisorApi`                       |
| `api-grpc-tls`                   | Shared TLS conversion for tonic                                       |
| `discover`                       | Base discovery contracts                                              |
| `discover-http`, `discover-grpc` | Outbound discovery transport                                          |
| `discover-tls`                   | Custom roots or mTLS for discovery                                    |
| `observe`                        | Logging configuration                                                 |
| `observe-*`                      | Journald, log compatibility, or timezone refresh                      |
| `prometheus-base`                | Prometheus namespace and base contracts                               |
| `prometheus`                     | Runner and Taskvisor-controller metrics bundle                        |
| `prometheus-*`                   | API, discovery, process, server, state, runner, or Taskvisor adapters |
| `prometheus-full`                | Every Prometheus adapter                                              |
| `taskvisor-*`                    | Forwarded Taskvisor integrations                                      |
| `tls`                            | Shared TLS and mTLS types                                             |
| `full`                           | Complete standard integration set                                     |

`exec-seccomp` provides the filter implementation.
Combine it with `exec-subprocess` to apply the filter to subprocess attempts.

`api-http` and `api-grpc` do not enable core.
Add `api-core-adapter` when the public API delegates to `SupervisorApi`.

`full` compiles the native containerd adapter.
Container execution remains Linux-only.

## Examples

From a cloned checkout, start with the direct Task lifecycle:

```bash
cargo run -p solti --example task_subprocess \
  --features core,exec-subprocess
```

Names identify the boundary:

- `task_*` calls the in-process Task lifecycle directly;
- `agent_*` assembles an API or discovery boundary;
- `operations_*` composes metrics, logging, and maintenance.

### Task lifecycle

| Example                                                                  | Features                       | Result                                                |
|--------------------------------------------------------------------------|--------------------------------|-------------------------------------------------------|
| [task_chain.rs](crates/solti/examples/task_chain.rs)                     | `chain,core,exec-subprocess`   | Conditional steps with failure recovery               |
| [task_subprocess.rs](crates/solti/examples/task_subprocess.rs)           | `core,exec-subprocess`         | Output, reconciliation, terminal status, and history  |
| [task_custom_workload.rs](crates/solti/examples/task_custom_workload.rs) | `core`                         | Application-owned `TcpProbe` GVK and runner           |
| [task_containerd.rs](crates/solti/examples/task_containerd.rs)           | `core,exec-containerd`         | Native containerd 2.x attempt; Linux runtime required |

### Agent boundaries

| Example                                                                  | Features                                                  | Result                                            |
|--------------------------------------------------------------------------|-----------------------------------------------------------|---------------------------------------------------|
| [agent_http.rs](crates/solti/examples/agent_http.rs)                     | `api-core-adapter,api-http,exec-subprocess`               | HTTP Task API, OpenAPI, and runnable `curl` calls |
| [agent_grpc.rs](crates/solti/examples/agent_grpc.rs)                     | `api-core-adapter,api-grpc,exec-subprocess`               | gRPC Task API, bearer auth, and `grpcurl` calls   |
| [agent_grpc_mtls.rs](crates/solti/examples/agent_grpc_mtls.rs)           | `api-core-adapter,api-grpc-tls,exec-subprocess`           | Anonymous rejection and authenticated mTLS client |
| [agent_http_discovery.rs](crates/solti/examples/agent_http_discovery.rs) | `api-core-adapter,api-http,discover-http,exec-subprocess` | Inbound Task API and outbound discovery heartbeat |

### Operations

| Example                                                                    | Features                                                             | Result                                          |
|----------------------------------------------------------------------------|----------------------------------------------------------------------|-------------------------------------------------|
| [operations_prometheus.rs](crates/solti/examples/operations_prometheus.rs) | `core,exec-subprocess,prometheus,prometheus-server,prometheus-state` | Supervised `/metrics` with real runtime samples |
| [operations_observe.rs](crates/solti/examples/operations_observe.rs)       | `core,exec-subprocess,observe-timezone-sync`                         | Logging and supervised timezone maintenance     |

`agent_http` and `agent_grpc` remain active until Ctrl-C.
They print commands that can be run from a second terminal.

`operations_prometheus` serves `http://127.0.0.1:9090/metrics` until Ctrl-C.
Set `SOLTI_METRICS_ADDR` to use another listen address.

`task_containerd` requires Linux and an accessible containerd 2.x daemon for a real attempt.

## Development

Vendor the pinned protobuf contracts after a fresh checkout:

```bash
task proto/vendor
```

The task fetches the revision pinned in [`Taskfile.yml`](Taskfile.yml).
Vendored Protobuf source trees are ignored by Git and included in published
transport crates. Each transport crate generates Rust bindings into Cargo's
`OUT_DIR` at build time; those artifacts are never committed.

Run the workspace checks:

```bash
task ci/fmt
task ci/check
task ci/clippy
task ci/test
task ci/docs
task ci/audit
task ci/bench-check
task ci/package
```

`task ci/publish/dry-run` is a separate, non-gating registry diagnostic. It
verifies each archive against the internal crate versions currently available
on crates.io; it does not stage the complete workspace release.

The release order is declared in [`.github/crates.txt`](.github/crates.txt).
Component crates are published before the `solti` umbrella crate.
Use the [release checklist](DEPLOY.md) before creating a version tag.

## Benchmarks

Process benchmarks live in the root [`benches/`](benches/README.md) workspace
package, outside product crates. They cover lifecycle, reconciliation,
execution, collections, API boundaries, and shutdown.

```bash
task rust:benchmark
```

The suite uses Taskvisor-style reports with named units and explicit timing
boundaries. See the [scenario map and run options](benches/README.md), including
the separately gated Linux containerd and host-policy cases.

## Contributing

Issues and pull requests are welcome.
Start with the relevant crate README.

Read the architecture guides before changing the [model](crates/solti-model/ARCHITECTURE.md), [core](crates/solti-core/ARCHITECTURE.md), or [execution](crates/solti-exec/ARCHITECTURE.md) boundaries.
Read the [Task API contract](crates/solti-api/CONTRACT.md) or [discovery contract](crates/solti-discover/CONTRACT.md) before changing wire behavior.
Read the [observability guide](crates/solti-observe/README.md) when configuring SDK logging.

Read the [contributing guide](https://github.com/soltiHQ/.github/blob/main/CONTRIBUTING.md) before a large change.

If Solti helps your project, a GitHub star helps other Rust developers find it.

<br>

<p align="center">
  <a href="https://github.com/soltiHQ">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/word/solti-word-light.svg">
      <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-dark.svg" alt="soltiHQ" height="84">
    </picture>
  </a>
</p>
