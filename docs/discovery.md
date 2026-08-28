---
title: Register an agent and send heartbeats
description: Advertise an agent API and runner capabilities through a supervised outbound discovery task.
---

# Register an agent and send heartbeats

`solti-discover` builds a periodic task that registers an agent and sends liveness updates to a control plane.
It returns a `TaskManifest` and `TaskRef`. It does not start the agent API, submit the task, or run the control-plane server.

## Separate the two endpoints

```text
Control plane ── Task API calls ──► AgentEndpoint

Agent discovery task ── heartbeat ──► ControlPlaneEndpoint
```

| Value | Owner and purpose |
|-------|-------------------|
| `AgentEndpoint` | The application supplies the API address, transport, and Task API version advertised to the control plane. |
| `ControlPlaneEndpoint` | The application supplies the outbound discovery address and discovery transport. |
| `AgentId` | The application assigns a stable identity and owns its uniqueness. The model validates its format. |
| `AgentCapabilities` | The runner router describes the routed workloads this agent exposes. |
| `DiscoverConfig` | `solti-discover` captures endpoint, identity, metadata, timing, credentials, and metrics settings. |
| Embedded task | `solti-core` commits its desired state; Taskvisor runs, retries, and cancels its attempts. |

The two transport choices are independent.
An HTTP Task API can use gRPC discovery, and a gRPC Task API can use HTTP discovery.
Discovery protocol v1 and Task API v1 are separate version identities.
Pass `solti_api::API_VERSION` for the API advertised by this build; it does not select the discovery protocol.

`AgentEndpoint::new` checks a non-empty address and an API version in `1..=i32::MAX`.
It does not bind that address or verify reachability from the control plane.
A successful heartbeat means that the control plane accepted that request, not that the advertised API or every workload has passed a readiness check.

## Select features

No `solti-discover` feature is enabled by default.
The base crate exposes protocol identity, errors, and metrics contracts.

| Need | Direct crate features | `solti` facade feature |
|------|-----------------------|------------------------|
| HTTP discovery | `solti-discover/http` | `discover-http` |
| HTTP with custom trust roots or client identity | `solti-discover/http,tls` | `discover-http-tls` |
| Plaintext gRPC discovery | `solti-discover/grpc` | `discover-grpc` |
| gRPC HTTPS or custom TLS | `solti-discover/grpc,tls` | `discover-grpc-tls` |
| Submit through core | `solti-core` | `core` |

The direct `tls` feature extends a transport. It does not enable HTTP or gRPC on its own.
HTTP HTTPS already uses platform roots without `tls`; custom material needs `tls`.
The documented gRPC HTTPS path needs `tls` even when using platform roots.
See [TLS and authentication](tls-and-authentication.md).

## Build from the running agent

Take capabilities from the same supervisor that serves the public Task API:

```rust
use std::sync::Arc;
use solti_core::SupervisorApi;
use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, UptimeSource, sync,
};
use solti_model::{AgentId, TaskId};

async fn add_discovery(
    supervisor: &SupervisorApi,
    uptime: Arc<dyn UptimeSource>,
    advertised_api: &str,
    control_plane: &str,
    task_revision: &str,
) -> Result<TaskId, Box<dyn std::error::Error>> {
    let config = DiscoverConfig::builder(
        AgentId::new("agent-1")?,
        "Agent 1",
        AgentEndpoint::new(
            advertised_api,
            AgentEndpointType::Http,
            solti_api::API_VERSION,
        )?,
        ControlPlaneEndpoint::new(control_plane, DiscoveryTransport::Http)?,
        supervisor.runner_capabilities(),
        30_000,
        task_revision,
    )
    .build()?;

    let (manifest, task_ref) = sync(config, uptime)?;
    let name = manifest.name().clone();
    supervisor.create_embedded_task(manifest, task_ref).await?;
    Ok(name)
}
```

This uses `solti-discover/http` and the public APIs of `solti-core`, `solti-model`, and `solti-api`.
The agent identity and 30-second interval are example choices, not generated identities or default interval settings.
The caller must already own the advertised HTTP server.

Construct `MonotonicUptime` at the agent lifecycle boundary and share it with the discovery task.
It measures elapsed whole seconds from construction without depending on wall-clock changes.
Use another `UptimeSource` when the application owns a different epoch.

`runner_capabilities()` returns the router's immutable snapshot.
Runner order preserves registration order and routing priority.
Each entry contains its name, static labels, and exact workload API-version/kind pairs.
Embedded tasks are not runner capabilities because they bypass routing.
An agent without routed runners explicitly advertises an empty capability set.

## Keep configuration and revision together

`DiscoverConfigBuilder::build` validates scalar settings.
`sync` then validates the selected transport and builds its reusable client.
Network connections remain lazy; constructing a task is not a successful registration.

The task captures its complete configuration and the supplied uptime source.
Only the timestamp and uptime are refreshed for each request.
When captured settings change, build another task and use `apply_embedded_task` with a new `task_revision`.
That includes metadata, endpoints, credentials, TLS material, capabilities, the metrics backend, and the uptime source or epoch.
Core cannot infer changes to an opaque captured `TaskRef` from an unchanged Embedded revision and manifest.

The generated resource name and slot are `solti-discover-sync`.
Its admission policy is `Replace`.
It is visible through the in-process core API and hidden by `SupervisorApiAdapter`, like other Embedded maintenance tasks.

| Setting | Required value or default |
|---------|---------------------------|
| Display name and revision | Non-empty after trimming. |
| `delay_ms` | Required, positive; its rounded-up heartbeat seconds must fit `i32`. |
| Connect timeout | 5000 ms by default; positive. |
| Request timeout | 30000 ms by default; positive. |
| Metadata | Empty by default. |
| Token and custom TLS | Absent by default. |
| Metrics | No-op by default. |
| Failure backoff | Equal jitter; first base `max(delay_ms / 2, 1)`, maximum `delay_ms * 3` with saturation, factor 2. |

The heartbeat interval advertised to the control plane is `ceil(delay_ms / 1000)` seconds.
Public settings ending in `_ms` remain milliseconds.

## Follow one attempt

```text
first attempt only: startup jitter below delay_ms
          ▼
wait for any remaining server hold
          ▼
stamp current time and uptime ──► send request
          ├── accepted ──► success ──► periodic delay_ms
          ├── retryable failure ──► TaskError::Fail ──► failure backoff
          └── permanent failure ──► TaskError::Fatal ──► stop
```

The startup jitter applies once per constructed task, not once per retry.
The success interval is a delay after an attempt finishes, not a wall-clock schedule.
Sleeps and transport awaits observe task cancellation.

The attempt timeout includes one interval, the maximum one-hour server hold, connect timeout, request timeout, and one second of overhead.
It is separate from the transport's request timeout.

A response with `success=true` accepts the sync and ignores any reason or retry hold in that response.
`success=false` is a retryable `DiscoverError::Rejected`.
Its reason is diagnostic text, not a machine-readable failure class.

A positive `retry_after_s` sets a monotonic hold deadline, capped at 3600 seconds.
Backoff still follows the failed attempt. The next request waits for the remainder of the hold if necessary.
Backoff and hold are overlapping constraints, not two delays added together.

## Handle transport and response failures

HTTP sends:

```text
POST {control-plane base path}/api/v1/discovery/sync
Content-Type: application/json
```

For example, a control-plane address of `https://control.example/control` produces `/control/api/v1/discovery/sync`.
HTTP endpoints must use `http` or `https`; query strings and fragments are rejected.
Redirects are disabled. One HTTP client is reused across attempts.

The discovery HTTP body follows the documented protobuf JSON encoding, unlike the Task API's CRD JSON.
Use the [discovery contract](../crates/solti-discover/CONTRACT.md#http-binding) for exact field encodings.
Successful response bodies are limited to 64 KiB. Non-success responses retain at most a 1 KiB diagnostic body preview.
The protocol does not define an aggregate discovery request-byte ceiling.

| Failure | Behavior |
|---------|----------|
| HTTP `401` or `403` | Permanent authentication failure. |
| HTTP `408`, `425`, `429`, or `5xx` | Retryable. |
| Other non-success HTTP status | Permanent. |
| Connection, timeout, body read, invalid UTF-8/JSON, or oversized successful response | Retryable runtime failure. |
| Protocol `success=false` | Retryable, with an optional server hold. |
| Invalid configuration or task construction | Returned before a task is available. |

HTTP client construction can itself fail in `sync`; Taskvisor cannot retry a task that was never returned.
An uptime outside the signed 64-bit protocol range fails the attempt permanently before the transport request or its metrics callbacks.

The documented gRPC endpoint is `/solti.discover.v1.DiscoverService/Sync`.
Its first attempt connects lazily; later attempts reuse the successful channel.
The [gRPC contract](../crates/solti-discover/CONTRACT.md#grpc-binding) lists permanent status codes and retryable failures.
Branch on `DiscoverError::retryability()`, not response reason strings.

## Own credentials and shutdown

A bearer token is sent with every sync and requires an HTTPS control-plane endpoint by default.
`sync` rejects a token over plaintext unless `allow_insecure_token_transport()` explicitly permits it.
That escape hatch emits a warning and is intended for controlled development or loopback use.
Plaintext without a token remains supported.

Discovery does not configure inbound Task API authentication.
The advertised server and outbound heartbeat client have separate credentials and TLS settings.

The application submits and manages the Embedded resource.
Core shutdown cancels the supervised discovery task; cancellation stops its awaited sleeps and request.
The task does not send an unregister request or persist registration state.
The control plane owns how it interprets missing heartbeats.

`with_metrics` attaches a `DiscoverMetricsBackend`.
It records transport attempts, success/failure duration, bounded failure categories, and server holds.
Startup jitter and hold waits are outside the measured request duration.
See [observability](observability.md) for the Prometheus adapter and callback failure boundary.

Source: [configuration](../crates/solti-discover/src/config.rs), [attempt lifecycle](../crates/solti-discover/src/tasks/sync.rs), [HTTP transport](../crates/solti-discover/src/tasks/transport/http.rs), [errors](../crates/solti-discover/src/errors.rs), and [public protocol contract](../crates/solti-discover/CONTRACT.md).

## Run the examples

The self-contained [HTTP heartbeat example](../crates/solti-discover/examples/http_sync.rs) binds a local mock control plane and executes one task attempt.
It demonstrates the discovery boundary, not core supervision:

```sh
cargo run -p solti-discover --example http_sync --features http
```

The complete [HTTP agent with discovery](../crates/solti/examples/agent_http_discovery.rs) runs the API, subprocess runner, core, and a supervised heartbeat:

```sh
SOLTI_CONTROL_PLANE=http://127.0.0.1:8090 \
  cargo run -p solti --example agent_http_discovery \
  --features api-core-adapter,api-http,discover-http,exec-subprocess
```

That command needs a compatible discovery HTTP v1 server at the selected control-plane endpoint.
The example does not start that server. Its API listens on `127.0.0.1:8085`.
See [serving the API](serving-api.md) and [building an agent](building-an-agent.md) for the wider application lifecycle.
