# Solti SDK

**Modular Rust toolkit for building task-orchestration agents.**

Pick what you need: run a subprocess with restart policies, build a headless scheduler, expose an HTTP/gRPC API, or connect to a [Podium](https://github.com/soltiHQ/podium) control-plane. Every layer is optional except `solti-model` (domain types) and `solti-core` (supervision).

Built on [taskvisor](https://github.com/soltiHQ/taskvisor).

## What you can build

- **Standalone scheduler**: `solti-core` + `solti-exec`. Submit tasks from code; the supervisor handles retries, timeouts, and backoff.
- **HTTP/gRPC service**: add `solti-api` to expose task management over the network. Pick HTTP (axum), gRPC (tonic), or both.
- **Managed agent**: add `solti-discover` to register with a Podium control-plane. The control-plane pushes specs, the agent executes.
- **TLS / mTLS everywhere**: `solti-tls` provides a single config shape for `solti-api` (server) and `solti-discover` (client). Same builder, paths or in-memory PEM, mTLS as a one-line knob.
- **Bearer-token auth**: one shared secret (`solti_model::Token`) authenticates both directions — `solti-discover` presents it to the control-plane, `solti-api` verifies inbound calls against it. Constant-time check, redacted in logs, orthogonal to TLS, enabled from a single config value.
- **Live-tail task output**: subscribe to a task's stdout/stderr over HTTP Server-Sent Events (`GET /api/v1/tasks/{id}/logs`) or gRPC server-streaming (`StreamTaskLogs`). One subscription covers all retries with explicit run-boundary markers; the agent never persists output.
- **Custom runner**: implement the `Runner` trait to execute tasks your way (WASM, containers, in-process functions). The router dispatches by label selectors.
- **Embedded tasks**: `TaskKind::Embedded` runs async Rust closures under the same supervision tree as subprocesses. Sweep, timezone sync, and discovery heartbeat use it internally.

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

Upper layers depend on lower ones; no cycles. The top row is entirely optional — use only what your agent needs.

## Crates

| Crate                                          | What it does                                                  | Required?                 |
|------------------------------------------------|---------------------------------------------------------------|---------------------------|
| [`solti-model`](crates/solti-model)            | Domain types: specs, policies, selectors, identifiers         | yes                       |
| [`solti-runner`](crates/solti-runner)          | Runner plugin trait, label-based routing, metrics interface   | yes                       |
| [`solti-exec`](crates/solti-exec)              | Subprocess runner: cgroups v2, capabilities, rlimits (Linux)  | if you run subprocesses   |
| [`solti-core`](crates/solti-core)              | Supervisor orchestration, in-memory state, sweep              | yes                       |
| [`solti-api`](crates/solti-api)                | HTTP/JSON and gRPC API layer (feature-gated)                  | if you need a network API |
| [`solti-discover`](crates/solti-discover)      | Agent registration and heartbeat to Podium control-plane      | if managed by Podium      |
| [`solti-tls`](crates/solti-tls)                | Shared TLS / mTLS config (paths or in-memory PEM)             | if `tls` feature enabled  |
| [`solti-observe`](crates/solti-observe)        | Structured logging with timezone sync                         | recommended               |
| [`solti-prometheus`](crates/solti-prometheus)  | Prometheus metrics backend                                    | if you need metrics       |

Each crate has its own README with a detailed reference.

## Quick start

### Prerequisites

- Rust 2024 edition (1.90+)
- Subprocess runner is Linux-only (cgroups v2, capabilities). The rest of the SDK is cross-platform.

### Minimal: run a task from code

No API, no discovery — just supervision and execution:

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
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;

    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        vec![],
        router,
        StateConfig::default(),
    )
    .await?;

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

    Ok(())
}
```

Enough for a headless scheduler, CI runner, or background job processor.

### With network API

Add `solti-api` to expose tasks over HTTP:

```rust
use std::sync::Arc;
use solti_api::{HttpApi, SupervisorApiAdapter};

let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
let app = HttpApi::new(handler).router();

let listener = tokio::net::TcpListener::bind("0.0.0.0:8085").await?;
axum::serve(listener, app).await?;
```

```bash
curl -X POST http://localhost:8085/api/v1/tasks -H 'Content-Type: application/json' -d '{"spec":{...}}'
curl http://localhost:8085/api/v1/tasks
curl http://localhost:8085/api/v1/tasks/{id}/runs
curl -N http://localhost:8085/api/v1/tasks/{id}/logs    # live-tail SSE
```

See [`api_v1.md`](crates/solti-api/api_v1.md) for the full endpoint reference.

### With control-plane

Register with [Podium](https://github.com/soltiHQ/podium) and receive specs remotely:

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

See [`examples/agentd-http`](examples/agentd-http) and [`examples/agentd-grpc`](examples/agentd-grpc) for full reference agents - one per transport.

### With TLS / mTLS

Enable the `tls` feature on `solti-api` (server) and/or `solti-discover` (client). One config shape feeds both:

```rust
use solti_tls::{ClientTlsConfig, ServerTlsConfig};

// Server: cert + key + optional client-CA for mTLS.
let server = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt") // omit for plain TLS
    .with_alpn(["h2"])
    .build()?;

// Client: trust roots + optional client cert for mTLS.
let client = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .client_cert_pem_file("/etc/solti/tls/agent.crt")
    .client_key_pem_file("/etc/solti/tls/agent.key")
    .build()?;
```

Plug `server` into `tonic`/`axum-server`, or pass `client` to `DiscoverConfigBuilder::with_tls(...)`. End-to-end demo: [`examples/tls-roundtrip`](examples/tls-roundtrip).

## Key features

**Supervision**: automatic restarts, configurable backoff (full / equal / decorrelated jitter), per-attempt timeouts, graceful cancellation via `CancellationToken`.

**Admission control**: duplicate submission on the same slot: drop, replace, or queue. Configurable per spec.

**Runner routing**: tasks carry label selectors, runners register with labels. Ship multiple runners in one binary.

**Subprocess isolation (Linux)**: cgroup v2 resource limits, capability dropping, rlimit enforcement.

**Embedded tasks**: async Rust closures supervised next to subprocesses. Used internally for sweep, timezone sync, and discovery heartbeat.

**Dual-transport API**: HTTP/JSON (axum) and gRPC (tonic) behind feature flags. Use one, both, or neither.

**Live-tail output**: stdout/stderr broadcast per task over SSE and gRPC server-streaming. Multi-run merge (retries inherit the channel), backpressure-aware (`Lagged` event on slow subscribers), zero-copy line payloads via `bytes::Bytes`.

**Observability**: structured logging (`tracing`, JSON / text / journald, local timezone), Prometheus metrics, lifecycle event subscribers.

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

External cancellation moves a task to `Canceled`.

## Development

```bash
cargo build --workspace
cargo test --workspace

# Reference agents
cargo run -p agentd-http     # HTTP transport, :8085
cargo run -p agentd-grpc     # gRPC transport, :50052
cargo run -p tls-roundtrip   # mTLS demo (HTTPS :18443 + gRPC :18444)

# Feature-gated builds
cargo build -p solti-api      --features http
cargo build -p solti-api      --features grpc
cargo build -p solti-api      --features grpc,tls
cargo build -p solti-discover --features http,tls
```

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
