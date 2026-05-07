# Solti SDK

**Modular Rust toolkit for building task-orchestration agents.**

Solti is a set of composable crates.

Pick what you need: run a subprocess with restart policies, build a headless scheduler, expose an HTTP/gRPC API, or connect to a [Podium](https://github.com/soltiHQ/podium) control-plane.
Every layer is optional except `solti-model` (domain types) and `solti-core` (supervision).

Built on top of [taskvisor](https://github.com/soltiHQ/taskvisor) supervision runtime.

## What you can build

The SDK doesn't prescribe a single topology. Examples of what fits naturally:

- **Standalone scheduler** `solti-core` + `solti-exec`. No network, no API. Submit tasks from code, let the supervisor handle retries, timeouts, and backoff.
- **HTTP/gRPC service** add `solti-api` to expose task management over the network. Feature-gated: pick HTTP (axum), gRPC (tonic), or both.
- **Managed agent** add `solti-discover` to register with a Podium control-plane. The control-plane pushes specs, the agent executes.
- **TLS / mTLS everywhere** `solti-tls` provides a single config shape for `solti-api` (server) and `solti-discover` (client). Same builder, paths or in-memory PEM, mTLS as a one-line knob.
- **Live-tail task output** subscribe to a task's stdout/stderr stream over HTTP Server-Sent Events (`GET /api/v1/tasks/{id}/logs`). One subscription covers all retries of a task with explicit run-boundary markers; nothing is persisted in the agent.
- **Custom runner** implement the `Runner` trait to execute tasks your way (WASM, containers, in-process functions). The router dispatches by label selectors.
- **Embedded tasks** `TaskKind::Embedded` lets you run async Rust closures under the same supervision tree as subprocesses. Sweep, timezone sync, and discovery heartbeat all work this way internally.

## Architecture

```text
┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
                       your agent binary
├───────────┬────────────────┬──────────────────┬───────────────┤
│ solti-api │ solti-discover │ solti-prometheus │ solti-observe │
│ HTTP/gRPC │  heartbeat     │     metrics      │    logging    │
├───────────┴────────────────┴──────────────────┴───────────────┤
│        solti-tls - shared TLS / mTLS config (optional)        │
├───────────────────────────────────────────────────────────────┤
│                          solti-core                           │
│                  SupervisorApi · state · sweep                │
├───────────────────────────────────────────────────────────────┤
│                         solti-runner                          │
│                  Runner trait · router · metrics              │
├───────────────────────────┬───────────────────────────────────┤
│       solti-exec          │            (future)               │
│       subprocess          │         wasm · container          │
├───────────────────────────┴───────────────────────────────────┤
│                         solti-model                           │
│            domain types · policies · selectors · specs        │
└───────────────────────────────────────────────────────────────┘
```

Dependencies flow downward: `model ← runner ← core ← api`.
No circular dependencies.
The top row is entirely optional - use only what your agent needs.
`solti-tls` is also optional and only pulled in when `solti-api`/`solti-discover` are built with the `tls` feature.

## Crates

| Crate                                         | What it does                                                 | Required?                       |
|-----------------------------------------------|--------------------------------------------------------------|---------------------------------|
| [`solti-model`](crates/solti-model)           | Domain types: task specs, policies, selectors, identifiers   | yes                             |
| [`solti-runner`](crates/solti-runner)         | Runner plugin trait, label-based routing, metrics interface  | yes                             |
| [`solti-exec`](crates/solti-exec)             | Subprocess runner: cgroups v2, capabilities, rlimits (Linux) | if you run subprocesses         |
| [`solti-core`](crates/solti-core)             | Supervisor orchestration, in-memory state, sweep             | yes                             |
| [`solti-api`](crates/solti-api)               | HTTP/JSON and gRPC API layer (feature-gated)                 | if you need a network API       |
| [`solti-discover`](crates/solti-discover)     | Agent registration and heartbeat to Podium control-plane     | if managed by Podium            |
| [`solti-tls`](crates/solti-tls)               | Shared TLS / mTLS config (paths or in-memory PEM)            | if you enable `tls` on api/disc |
| [`solti-observe`](crates/solti-observe)       | Structured logging with timezone sync                        | recommended                     |
| [`solti-prometheus`](crates/solti-prometheus) | Prometheus metrics backend                                   | if you need metrics             |

## Quick start

### Prerequisites

- Rust 2024 edition (1.85+)
- Protobuf compiler (vendored via `protoc-bin-vendored`)
- Linux only for `solti-exec` subprocess runner (cgroups v2, capabilities). The rest of the SDK is cross-platform.

### Minimal: run a task from code

No API server, no discovery, no control-plane — just supervision and execution:

```rust
use solti_core::{StateConfig, SupervisorApi};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionPolicy, Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind,
    TaskSpec,
};
use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, SupervisorConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Register a runner backend
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;

    // 2. Create the supervisor
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        vec![],
        router,
        StateConfig::default(),
    )
        .await?;

    // 3. Submit a task
    let kind = TaskKind::Subprocess(SubprocessSpec {
        mode: SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello world".into()],
        },
        env: TaskEnv::new(),
        cwd: None,
        fail_on_non_zero: Flag::enabled(),
    });
    let spec = TaskSpec::builder("hello", kind, 30_000u64)
        .restart(RestartPolicy::Never)
        .admission(AdmissionPolicy::Replace)
        .build()?;

    let task_id = supervisor.submit(&spec).await?;
    println!("submitted: {task_id}");

    // 4. Query state
    if let Some(task) = supervisor.get_task(&task_id) {
        println!("phase: {:?}", task.status.phase);
    }

    Ok(())
}
```

This is all you need for a headless scheduler, a CI runner, or a background job processor.

### With network API

Add `solti-api` to expose tasks over HTTP:

```rust
use std::sync::Arc;
use solti_api::{HttpApi, SupervisorApiAdapter};

// ... supervisor setup as above ...

let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
let app = HttpApi::new(handler).router();

let listener = tokio::net::TcpListener::bind("0.0.0.0:8085").await?;
axum::serve(listener, app).await?;
```

```bash
curl -X POST http://localhost:8085/api/v1/tasks -H "Content-Type: application/json" -d '{"spec": {...}}'
curl http://localhost:8085/api/v1/tasks
curl http://localhost:8085/api/v1/tasks/{task_id}/runs
```

See [`api_v1.md`](crates/solti-api/api_v1.md) for the full endpoint reference.

### With control-plane

Add `solti-discover` to register with [Podium](https://github.com/soltiHQ/podium) and receive specs remotely:

```rust
use solti_api::API_VERSION;
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_model::AgentId;

let config = DiscoverConfig::builder(
    AgentId::new("worker-001"),
    "worker",
    "http://this-host:8085",
    "http://podium:8082",
    DiscoveryTransport::Http,
    10_000, // heartbeat interval (ms)
    API_VERSION,
)
.build()?;

let (task, spec) = solti_discover::sync(config)?;
supervisor.submit_with_task(task, &spec).await?;
```

See [`examples/agentd-http`](examples/agentd-http) and [`examples/agentd-grpc`](examples/agentd-grpc) for complete reference agents - one per transport.

### With TLS / mTLS

Enable the `tls` feature on `solti-api` (server) and/or `solti-discover` (client). One config shape feeds both transports:

```rust
use solti_tls::{ClientTlsConfig, ServerTlsConfig};

// Server side: cert + key + optional client-CA for mTLS.
let server = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")  // omit for plain TLS
    .with_alpn(["h2"])
    .build()?;

// Client side: trust roots + optional client cert for mTLS.
let client = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .client_cert_pem_file("/etc/solti/tls/agent.crt")
    .client_key_pem_file("/etc/solti/tls/agent.key")
    .build()?;
```

Plug `server` into the binary's `tonic`/`axum-server` setup (see `solti-tls` README), or pass `client` to `DiscoverConfigBuilder::with_tls(...)`. Runnable end-to-end demo: [`examples/tls-roundtrip`](examples/tls-roundtrip) (`cargo run -p tls-roundtrip` → `mTLS round-trip OK`).

## Dashboards

Pre-built Grafana dashboards live in [`soltiHQ/dashboards`](https://github.com/soltiHQ/dashboards).

Setup options:
- **Import via Grafana UI**: paste the dashboard ID once it's listed on `grafana.com`, or upload the JSON from the dashboards repo.
- **Local provisioning**: clone the repo and mount `solti/` into Grafana. See the [dashboards README](https://github.com/soltiHQ/dashboards#usage) for `docker-compose` snippets.

## Key features

**Supervision**: tasks are supervised by [taskvisor](https://github.com/soltiHQ/taskvisor): automatic restarts, configurable backoff (full/equal/decorrelated jitter), per-attempt timeouts, and graceful cancellation via `CancellationToken`.

**Admission control**: when a duplicate submission arrives for the same slot: drop it, replace the running task, or queue it. Configurable per-spec.

**Runner routing**: tasks carry label selectors, runners register with labels. The `RunnerRouter` dispatches to the right backend. Ship multiple runners in one binary.

**Subprocess isolation** (Linux): cgroup v2 resource limits, Linux capability dropping, rlimit enforcement. Processes are supervised, not fire-and-forget.

**Embedded tasks** — async Rust closures run under the same supervision tree. Used internally for sweep, timezone sync, and discovery heartbeat. Available to your code via `TaskKind::Embedded`.

**Dual-transport API**: HTTP/JSON (axum) and gRPC (tonic) behind feature flags. Use one, both, or neither.

**Observability**: structured logging (`tracing` + `tracing-subscriber`, JSON / text / journald), local timezone in timestamps, Prometheus metrics, lifecycle event subscribers.

## Task lifecycle

```text
         submit
           │
           ▼
       ┌────────┐
       │Pending │
       └───┬────┘
           │ runner picks up
           ▼
       ┌────────┐    timeout     ┌─────────┐
       │Running │───────────────►│ Timeout │
       └───┬────┘                └─────────┘
           │
     ┌─────┴───────┐
     │             │
     ▼             ▼
┌─────────┐  ┌────────┐                       ┌───────────┐
│Succeeded│  │ Failed │── retries exhausted──►│ Exhausted │
└─────────┘  └────────┘                       └───────────┘
                 │
                 │ restart policy
                 ▼
             ┌────────┐
             │Running │  (next attempt)
             └────────┘
```

Tasks can also be externally cancelled → `Canceled`.

## Project structure

```
sdk/
├── crates/
│   ├── solti-model/       # Domain types (zero runtime deps)
│   ├── solti-runner/      # Runner trait + routing
│   ├── solti-exec/        # Subprocess execution backend
│   ├── solti-core/        # Supervisor orchestration
│   ├── solti-api/         # HTTP & gRPC API (feature-gated)
│   ├── solti-discover/    # Agent discovery (optional)
│   ├── solti-tls/         # Shared TLS / mTLS config (optional)
│   ├── solti-observe/     # Logging
│   └── solti-prometheus/  # Metrics backend
├── examples/
│   ├── agentd-http/       # Reference agent: HTTP API + discovery
│   ├── agentd-grpc/       # Reference agent: gRPC API + discovery
│   └── tls-roundtrip/     # mTLS demo (axum-server + tonic + reqwest)
├── LICENSE                # Apache-2.0
└── CODE_OF_CONDUCT.md
```

Each crate has its own README with detailed documentation.

## Development

```bash
cargo build --workspace
cargo test --workspace

# Run a reference agent
cargo run -p agentd-http     # HTTP transport, :8085
cargo run -p agentd-grpc     # gRPC transport, :50052
cargo run -p tls-roundtrip   # mTLS demo (HTTPS :18443 + gRPC :18444)

# Feature-gated builds
cargo build -p solti-api --features http
cargo build -p solti-api --features grpc
cargo build -p solti-api --features grpc,tls           # gRPC + TLS adapter
cargo build -p solti-discover --features http,tls      # HTTP client + TLS
```

## Status

Active development.

| Runner backend | Status           |
|----------------|------------------|
| Subprocess     | Production-ready |
| Container      | Planned          |
| WebAssembly    | Planned          |

## License

[Apache License, Version 2.0](LICENSE)

## Contributing

Found a bug? Have an idea? [Open an issue](https://github.com/soltiHQ/sdk/issues) or send a pull request.

<div>
  <a href="https://docs.rs/solti-core/latest/solti_core/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
  <a href="https://github.com/soltiHQ/dashboards"><img alt="Dashboards" src="https://img.shields.io/badge/Dashboards-f46800?style=for-the-badge&logo=grafana&logoColor=white"></a>
  <a href="https://github.com/soltiHQ/taskvisor"><img alt="Taskvisor" src="https://img.shields.io/badge/Taskvisor-2c3e50?style=for-the-badge&logo=rust&logoColor=white"></a>
</div>