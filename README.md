# Solti SDK

**Modular Rust toolkit for building task-orchestration agents.**

Pick what you need: run a subprocess with restart policies, build a headless scheduler, expose an HTTP/gRPC API, or connect to a [Podium](https://github.com/soltiHQ/podium) control-plane. The foundation is `solti-model` + `solti-runner` + `solti-core`; every layer above it is optional.

Built on [taskvisor](https://github.com/soltiHQ/taskvisor).

## What you can build

- **Standalone scheduler**: `solti-core` + `solti-exec`. Submit tasks from code; the supervisor handles retries, timeouts, and backoff.
- **HTTP/gRPC service**: add `solti-api` to expose task management over the network. Pick HTTP (axum), gRPC (tonic), or both.
- **Managed agent**: add `solti-discover` to register with a Podium control-plane. The control-plane reaches the advertised agent API to manage tasks.
- **TLS / mTLS everywhere**: `solti-tls` provides shared PEM sources, identities, and trust roots. Server and client configs stay separate.
- **Bearer-token auth**: one shared secret (`solti_model::Token`) authenticates both directions — `solti-discover` presents it to the control-plane, `solti-api` verifies inbound calls against it. Constant-time check, redacted in logs, orthogonal to TLS, enabled from a single config value.
- **Live-tail task output**: subscribe to a task's stdout/stderr over HTTP Server-Sent Events (`GET /apis/solti.io/v1/tasks/{name}/logs`) or gRPC server-streaming (`StreamTaskLogs`). One live subscription can span retries and includes best-effort run-boundary markers; the agent never persists or replays output.
- **Custom runner**: implement the `Runner` trait to execute tasks your way (WASM, containers, in-process functions). The router dispatches by workload GVK and optional label selectors.
- **Embedded tasks**: `TaskWorkload::Embedded` runs async Rust closures under the same supervisor as subprocesses. Timezone sync, discovery heartbeat, and the metrics server use it internally.

## Architecture

`solti` is an optional feature-and-namespace façade above the component graph.
It contains no runtime logic, and component crates never depend on it.

```text
your agent binary
├── solti-api
│   ├── http / grpc ────────────────► solti-model
│   ├── core-adapter ───────────────► solti-core
│   └── grpc-tls ───────────────────► solti-tls
├── solti-discover
│   ├── http / grpc ────────────────► solti-model + taskvisor
│   └── tls ────────────────────────► solti-tls
├── solti-observe
│   └── timezone-sync ──────────────► solti-model + taskvisor
└── solti-prometheus ───────────────► feature-selected producer adapters

solti-core ──► solti-runner ──► solti-model
solti-exec ──► solti-runner + solti-model + taskvisor
```

Arrows point at the dependency; the graph has no cycles. The top row is entirely optional — use only what your agent needs. `solti-api` depends on core only through the explicit `core-adapter` feature; its base handler and transport crates are core-independent. `solti-discover` has no core dependency. Two crates live beside the stack rather than in it: `solti-exec` is a plugin — it depends on `solti-runner` (implements the `Runner` trait) and your binary registers it into the router. `solti-core` never depends on it. Alternative runners (WASM, containers, in-process) slot in the same way. `solti-tls` is a standalone utility with no SDK dependencies, pulled in by `solti-api/grpc-tls` or `solti-discover/tls`. `solti-prometheus` implements the metrics traits of `solti-runner` and, behind feature flags, those of `solti-api` / `solti-discover`.

## Crates

| Crate                                          | What it does                                                  | Required?                 |
|------------------------------------------------|---------------------------------------------------------------|---------------------------|
| [`solti`](crates/solti)                        | Optional umbrella with feature forwarding and namespaces      | recommended for binaries  |
| [`solti-model`](crates/solti-model)            | Domain types: specs, policies, selectors, identifiers         | yes                       |
| [`solti-runner`](crates/solti-runner)          | Runner plugin trait, GVK routing, optional label selectors    | yes                       |
| [`solti-exec`](crates/solti-exec)              | Execution backends and host-process hardening controls         | if you execute workloads  |
| [`solti-core`](crates/solti-core)              | Supervisor orchestration, in-memory state, retention           | yes                       |
| [`solti-api`](crates/solti-api)                | HTTP/JSON and gRPC API layer (feature-gated)                  | if you need a network API |
| [`solti-discover`](crates/solti-discover)      | Agent registration and heartbeat to Podium control-plane      | if managed by Podium      |
| [`solti-tls`](crates/solti-tls)                | TLS / mTLS primitives and client/server configs               | if a TLS feature is enabled |
| [`solti-observe`](crates/solti-observe)        | Structured logging with timezone sync                         | recommended               |
| [`solti-prometheus`](crates/solti-prometheus)  | Prometheus metrics backend                                    | if you need metrics       |

Each crate has its own README with a detailed reference.

## Quick start

### Prerequisites

- Rust 2024 edition (1.90+)
- Subprocess execution is cross-platform. Linux adds cgroups v2, capabilities, seccomp, and namespaces. These controls provide hardening, not a complete sandbox.

### Minimal: run a task from code

No API, no discovery — just supervision and execution:

```rust
use std::time::Duration;
use solti_core::{StateConfig, SupervisorApi};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionPolicy, Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest,
    TaskSpec, TaskWorkload,
};
use solti_runner::{BuildContext, RunnerRouter};
use taskvisor::{ControllerConfig, SupervisorConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut router = RunnerRouter::new().with_context(BuildContext::default());
    register_subprocess_runner(&mut router, "default")?;

    let supervisor = SupervisorApi::builder(router)
        .with_runtime_config(SupervisorConfig::default())
        .with_controller_config(ControllerConfig::default())
        .with_state_config(StateConfig::default())
        .start()
        .await?;

    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello world".into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("hello", workload, 30_000u64) // per-attempt timeout (ms)
        .restart(RestartPolicy::Never)
        .admission(AdmissionPolicy::Replace)
        .build()?;

    let created = supervisor
        .create_task(TaskManifest::new("hello", spec)?)
        .await?;
    let task_id = created.name().clone();
    println!("submitted: {task_id}");

    // create_task() commits desired state. Observe reconciliation asynchronously.
    let phase = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            if let Some(task) = supervisor.get_task(&task_id) {
                let phase = task.status().phase();
                if phase.is_terminal() {
                    break phase;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    println!("finished: {task_id} ({phase})");

    supervisor.shutdown().await?;
    Ok(())
}
```

Enough for a headless scheduler, CI runner, or background job processor.

### With network API

Enable the `http` feature on `solti-api` to expose tasks over HTTP. Keep the
same supervisor composition above, but replace the one-shot wait/shutdown tail
with a long-running server. The supervisor already owns and connects live
output:

```rust
use std::sync::Arc;
use solti_api::{HttpApi, SupervisorApiAdapter};

let supervisor = Arc::new(supervisor);
let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
let app = HttpApi::new(handler).router();

let listener = tokio::net::TcpListener::bind("0.0.0.0:8085").await?;
axum::serve(listener, app)
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
supervisor.shutdown().await?;
```

```bash
curl -X POST http://localhost:8085/apis/solti.io/v1/tasks -H 'Content-Type: application/json' --data-binary @task.json
curl http://localhost:8085/apis/solti.io/v1/tasks
curl http://localhost:8085/apis/solti.io/v1/tasks/{name}/runs
curl -N http://localhost:8085/apis/solti.io/v1/tasks/{name}/logs
```

See [`solti-api`](crates/solti-api) for the endpoint and wire-contract reference.

### With control-plane

Register with [Podium](https://github.com/soltiHQ/podium) and receive specs remotely:

```rust
use std::sync::Arc;
use solti_api::API_VERSION;
use solti_core::SupervisorApi;
use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, MonotonicUptime,
};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::AgentId;
use solti_runner::RunnerRouter;

let uptime = Arc::new(MonotonicUptime::new());
let mut router = RunnerRouter::new();
register_subprocess_runner(&mut router, "default")?;
let capabilities = router.capabilities();
let supervisor = SupervisorApi::builder(router).start().await?;

let config = DiscoverConfig::builder(
    AgentId::new("worker-001")?,
    "worker",
    AgentEndpoint::new(
        "http://this-host:8085",
        AgentEndpointType::Http,
        API_VERSION,
    )?,
    ControlPlaneEndpoint::new("http://podium:8082", DiscoveryTransport::Http)?,
    10_000,
    "worker-runtime@1",
)
.capabilities(capabilities)
.build()?;

let (manifest, task_ref) = solti_discover::sync(config, uptime)?;
supervisor.create_embedded_task(manifest, task_ref).await?;
```

See [`examples/agentd-http`](examples/agentd-http) and [`examples/agentd-grpc`](examples/agentd-grpc) for full reference agents - one per transport.

### With TLS / mTLS

Enable `grpc-tls` on `solti-api` (gRPC server) and/or `tls` on
`solti-discover` (client). Shared PEM types feed separate server and client configs:

```rust
use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};

// Server: cert + key + optional client-CA for mTLS.
let server = ServerTlsConfig::new(TlsIdentity::from_pem_files(
    "/etc/solti/tls/server.crt",
    "/etc/solti/tls/server.key",
))
.require_client_auth(TrustRoots::from_pem_file(
    "/etc/solti/tls/clients-ca.crt",
)); // omit require_client_auth for plain TLS

// Client: trust roots + optional client cert for mTLS.
let client = ClientTlsConfig::new(TrustRoots::from_pem_file(
    "/etc/solti/tls/control-plane-ca.crt",
))
.with_identity(TlsIdentity::from_pem_files(
    "/etc/solti/tls/agent.crt",
    "/etc/solti/tls/agent.key",
));
```

Plug `server` into `tonic`/`axum-server`, or pass `client` to `DiscoverConfigBuilder::with_tls(...)`. End-to-end demo: [`examples/tls-roundtrip`](examples/tls-roundtrip).

## Key features

**Supervision**: automatic restarts, configurable backoff (full / equal / decorrelated jitter), per-attempt timeouts, and cooperative cancellation through `taskvisor::TaskContext`.

**Admission control**: a new submission targeting a busy slot can be dropped, replace the current owner, or wait in a queue. Configurable per spec.

**Runner routing**: runners declare supported workload GVKs; an optional runner selector filters matching runners by labels.

**Subprocess controls**: POSIX rlimits plus Linux cgroup v2 limits, capability dropping, seccomp, and namespaces.

**Embedded tasks**: async Rust closures supervised next to subprocesses. Used internally for timezone sync, discovery heartbeat, and the metrics server.

**Dual-transport API**: HTTP/JSON (axum) and gRPC (tonic) behind feature flags. Use one, both, or neither.

**Live-tail output**: stdout/stderr broadcast per task over SSE and gRPC server-streaming. Multi-run merge (retries inherit the channel), bounded and lossy delivery (`Lagged` reports events missed by slow subscribers), zero-copy line payloads via `bytes::Bytes`.

**Observability**: structured logging (`tracing`, JSON / text / journald, local timezone), Prometheus metrics, lifecycle event subscribers.

## Task lifecycle

```text
Pending ──► Running ──► attempt outcome: Succeeded | Failed | Timeout
               ▲                              │
               └──── restart policy ──────────┤
                                              │ lifecycle joins
                                              ▼
                         Succeeded | Failed | Timeout | Exhausted | Canceled
```

`Succeeded`, `Failed`, and `Timeout` may describe an attempt and be followed by
another `Running` attempt: `Always` can restart success, while `OnFailure` can
restart failures and timeouts. Fatal errors or panics may finalize as `Failed`, a
final timeout can remain `Timeout`, and a spent retry budget becomes `Exhausted`.
The joined lifecycle outcome determines the retained resource phase; attempt
history remains available through `TaskRun`.

## Development

Vendor the pinned protobuf contracts after a fresh checkout:

```bash
task proto/vendor
```

This step requires [Task](https://taskfile.dev), Docker, and network access.
Cargo builds use only the resulting local files.

Build and test the workspace:

```bash
cargo build --workspace
cargo test --workspace

# Reference agents
cargo run -p agentd-http     # HTTP transport, :8085
cargo run -p agentd-grpc     # gRPC transport, :50052
cargo run -p tls-roundtrip   # mTLS demo (HTTPS :18443 + gRPC :18444)
cargo run -p podium -- --config examples/podium/config.toml   # config-driven Podium agent

# Feature-gated builds
cargo build -p solti-api      --features http
cargo build -p solti-api      --features grpc
cargo build -p solti-api      --features grpc-tls
cargo build -p solti-discover --features http,tls
```

The generated trees live under `crates/solti-api/proto/` and `crates/solti-discover/proto/`.
They are ignored by Git.
The revision is pinned in [`Taskfile.yml`](Taskfile.yml).
Update `proto_ref` after publishing a compatible [`soltiHQ/proto`](https://github.com/soltiHQ/proto) tag.

## Dashboards

Pre-built Grafana dashboards live in [`soltiHQ/dashboards`](https://github.com/soltiHQ/dashboards) - import via the Grafana UI by ID, or clone and mount `solti/` into Grafana ([dashboards README](https://github.com/soltiHQ/dashboards#usage)).

## License

[Apache License, Version 2.0](LICENSE)

## Contributing

Found a bug? Have an idea? [Open an issue](https://github.com/soltiHQ/sdk/issues) or send a PR.

<div>
  <a href="https://docs.rs/solti-core/latest/solti_core/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
  <a href="https://github.com/soltiHQ/dashboards"><img alt="Dashboards" src="https://img.shields.io/badge/Dashboards-f46800?style=for-the-badge&logo=grafana&logoColor=white"></a>
  <a href="https://github.com/soltiHQ/taskvisor"><img alt="Taskvisor" src="https://img.shields.io/badge/Taskvisor-2c3e50?style=for-the-badge&logo=rust&logoColor=white"></a>
</div>
