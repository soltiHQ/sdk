# solti-discover

Agent registration and heartbeat for a Solti control plane.

`solti-discover` builds one embedded periodic task. The task advertises the
agent API and sends liveness data through HTTP or gRPC.

## Boundaries

The inbound and outbound endpoints are separate:

- `AgentEndpoint` describes the API exposed by the agent.
- `ControlPlaneEndpoint` describes the outbound discovery connection.
- `DiscoveryTransport` selects the outbound HTTP or gRPC adapter.

An agent can expose HTTP and sync through gRPC. The reverse combination also
works when both features are enabled.

Discovery uses protocol v1. `AgentEndpoint::api_version` is only the version of
the API advertised by the agent. It does not change the discovery endpoint.

## Example

```rust,no_run
use std::sync::Arc;

use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, MonotonicUptime,
};
use solti_model::AgentId;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = DiscoverConfig::builder(
        AgentId::new("agent-1")?,
        "agent-1",
        AgentEndpoint::new(
            "http://127.0.0.1:8085",
            AgentEndpointType::Http,
            1,
        )?,
        ControlPlaneEndpoint::new(
            "http://127.0.0.1:9000",
            DiscoveryTransport::Http,
        )?,
        30_000,
        "agent-runtime@1",
    )
    .build()?;

    let uptime = Arc::new(MonotonicUptime::new());
    let (_manifest, _task_ref) = solti_discover::sync(config, uptime)?;
    Ok(())
}
```

`task_revision` identifies the complete runtime intent captured by the embedded
task. Change it when the discovery config changes before applying the task
again.

## Capabilities

`DiscoverConfigBuilder::capabilities` accepts an immutable
`AgentCapabilities` snapshot. It contains each registered runner's name,
routing labels and exact workload GVKs.

In an agent composition, call `RunnerRouter::capabilities()` after registering
all runners. Pass the returned snapshot to the discovery config before
`build()`. This avoids a second capability list in the binary.

Runner order matches routing priority. Workload GVKs use canonical order.
Embedded workloads are not included because they do not use a runner. The
default snapshot has no runners.

## Features

| Feature | Purpose |
|---------|---------|
| `grpc`  | gRPC discovery v1 client |
| `http`  | HTTP/JSON discovery v1 client |
| `tls`   | Custom roots and mTLS through `solti-tls`; gRPC platform TLS |

No feature is enabled by default. The base crate contains the error and metrics
contracts without compiling protobuf tools.

HTTP supports HTTPS with platform roots under the `http` feature. `tls` adds
custom roots and an optional client identity. Custom TLS requires an `https`
control-plane endpoint.

gRPC HTTPS requires the `tls` feature. Without custom roots it uses platform
roots.

## Authentication

`with_token` attaches the same bearer value to every sync:

- HTTP uses `Authorization: Bearer <token>`.
- gRPC uses `authorization` metadata.

The header is validated when the transport adapter is created. A token over an
`http` endpoint produces a plaintext warning.

## Retry behavior

Every error has a [`Retryability`](https://docs.rs/solti-discover/latest/solti_discover/enum.Retryability.html):

- connection, timeout, throttling and server failures are retryable;
- invalid config, authentication and permanent client errors are permanent;
- `success = false` remains retryable and may include `retry_after_s`.

HTTP retries `408`, `425`, `429` and `5xx`. Other non-success statuses are
permanent. gRPC retries transient transport statuses and stops on permanent
client statuses.

The server hold uses a monotonic deadline and is capped at one hour. Taskvisor
applies its own backoff before the next task attempt.

## Task policy

The generated manifest uses:

- `TaskWorkload::Embedded`;
- `RestartPolicy::periodic(delay_ms)`;
- configurable backoff;
- `AdmissionPolicy::Replace`;
- slot `solti-discover-sync`.

The attempt timeout covers startup jitter, the maximum server hold, connection
setup and the request timeout.

## Uptime

The host owns the uptime epoch. `MonotonicUptime` starts at construction and is
not affected by wall-clock changes. A custom `UptimeSource` can provide another
host-owned definition.

## Protocol

The protobuf contract is in
[`proto/solti/discover/v1/discovery.proto`](proto/solti/discover/v1/discovery.proto).
The HTTP endpoint is always `POST /api/v1/discovery/sync`.

Generated request and response structs are internal. The public API exposes the
domain config and task factory instead of wire types.
