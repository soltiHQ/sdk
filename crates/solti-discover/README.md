# solti-discover

`solti-discover` builds agent registration and heartbeat work for a Solti control plane.
It returns one embedded task for periodic discovery sync.
The task supports HTTP/JSON or gRPC.

Use this crate when an agent binary must advertise its API, runner capabilities, and liveness.
The crate does not run a server or submit the task.
The application owns both operations.

## Quick start

Build an HTTP discovery task:

```rust,no_run
use std::sync::Arc;

use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, MonotonicUptime,
};
use solti_model::AgentId;

fn discovery_task() -> Result<(), Box<dyn std::error::Error>> {
    let config = DiscoverConfig::builder(
        AgentId::new("agent-1")?,
        "Agent 1",
        AgentEndpoint::new(
            "http://127.0.0.1:8085",
            AgentEndpointType::Http,
            1,
        )?,
        ControlPlaneEndpoint::new(
            "https://control.example",
            DiscoveryTransport::Http,
        )?,
        30_000,
        "discovery-config@1",
    )
    .build()?;

    let uptime = Arc::new(MonotonicUptime::new());
    let (manifest, task_ref) = solti_discover::sync(config, uptime)?;

    assert_eq!(manifest.name().as_str(), "solti-discover-sync");
    let _ = task_ref;
    Ok(())
}
```

Submit the returned manifest and `TaskRef` through `solti-core`.
Use `create_embedded_task` for a new resource.
Use `apply_embedded_task` when the desired config changes.

## What it does

- separates the advertised agent API from the outbound control-plane connection;
- builds one reusable HTTP or gRPC transport client;
- sends agent identity, endpoint, metadata, capabilities, platform, and liveness;
- classifies failures as retryable or permanent;
- applies server-advised retry holds;
- records discovery attempts through an optional metrics backend;
- returns a Taskvisor-compatible embedded task and `TaskManifest`.

## Inputs and outputs

| API or value                  | Input                                         | Output                                    |
|-------------------------------|-----------------------------------------------|-------------------------------------------|
| `AgentEndpoint::new`          | Address, endpoint type, API version           | Advertised agent endpoint                 |
| `ControlPlaneEndpoint::new`   | Control-plane address and discovery transport | Outbound discovery endpoint               |
| `DiscoverConfig::builder`     | Identity, endpoints, interval, task revision  | Config builder                            |
| `capabilities`                | `AgentCapabilities` snapshot                  | Runner capabilities sent with every sync  |
| `with_token`                  | `Token`                                       | Bearer authentication                     |
| `with_tls`                    | `solti_tls::ClientTlsConfig`                  | Custom roots and optional client identity |
| `with_metrics`                | `DiscoverMetricsHandle`                       | Discovery lifecycle metrics               |
| `sync`                        | `DiscoverConfig` and `Arc<dyn UptimeSource>`  | Embedded `TaskManifest` and `TaskRef`     |
| `DiscoverError::retryability` | Discovery failure                             | `Retryable` or `Permanent`                |

## Features

| Feature | Default | Effect                                                  |
|---------|---------|---------------------------------------------------------|
| `http`  | Off     | Enables HTTP/JSON discovery v1                          |
| `grpc`  | Off     | Enables gRPC discovery v1                               |
| `tls`   | Off     | Enables custom TLS and gRPC HTTPS support               |

No feature is enabled by default.
The base crate exposes only error and metrics contracts.

`tls` extends enabled transport features.
It does not enable `http` or `grpc` by itself.

## Configuration

`DiscoverConfig::builder` requires:

| Value                    | Rule                                                     |
|--------------------------|----------------------------------------------------------|
| `agent_id`               | Valid `AgentId`                                          |
| `name`                   | Non-empty after trimming                                 |
| `agent_endpoint`         | Non-empty address and API version from `1` to `i32::MAX` |
| `control_plane`          | Non-empty outbound endpoint                              |
| `delay_ms`               | Greater than zero and representable as wire seconds      |
| `task_revision`          | Non-empty after trimming                                 |

Optional settings use these defaults:

| Setting                  | Default                                                  |
|--------------------------|----------------------------------------------------------|
| `metadata`               | Empty                                                    |
| `capabilities`           | No runners                                               |
| `backoff`                | Equal jitter, half interval to three intervals, factor 2 |
| `connect_timeout_ms`     | 5 seconds                                                |
| `request_timeout_ms`     | 30 seconds                                               |
| Metrics                  | `NoOpDiscoverMetrics`                                    |
| Bearer token             | None                                                     |
| Custom TLS               | None                                                     |

`delay_ms`, `connect_timeout_ms`, and `request_timeout_ms` use milliseconds.
The heartbeat interval sent on the wire uses seconds rounded up.

`task_revision` identifies the complete runtime intent captured by the embedded task.
Change it when any captured discovery setting changes.
This lets `solti-core` reconcile an otherwise identical embedded workload.

## Capabilities

Pass the snapshot returned by `RunnerRouter::capabilities()` after runner registration:

```text
registered runners
       ▼
RunnerRouter::capabilities()
       ▼
DiscoverConfigBuilder::capabilities()
       ▼
discovery SyncRequest
```

Each runner contributes:

- its unique name;
- static routing labels;
- exact workload `apiVersion` and `kind` values.

Runner order preserves routing priority.
Workload GVKs use canonical order.
Embedded workloads are absent because they bypass runner routing.

The default capability snapshot has no runners.

## Authentication and TLS

`with_token` sends the same bearer token with every sync:

- HTTP uses the `Authorization` header;
- gRPC uses `authorization` metadata.

The selected adapter validates the encoded value before the task starts.
Using a token over a plaintext endpoint emits a warning.

| Transport | Endpoint | Trust behavior                                      |
|-----------|----------|-----------------------------------------------------|
| HTTP      | `http`   | Plaintext                                           |
| HTTP      | `https`  | Platform roots                                      |
| HTTP      | `https`  | Custom roots or mTLS through `with_tls`             |
| gRPC      | `http`   | Plaintext                                           |
| gRPC      | `https`  | Platform roots with feature `tls`                   |
| gRPC      | `https`  | Custom roots or mTLS through `with_tls`             |

Custom TLS requires an `https` control-plane endpoint.

## Retry behavior

Every `DiscoverError` exposes `retryability()`:

- connection, timeout, throttling, parse, and server failures are retryable;
- invalid config, invalid task construction, authentication, and permanent client failures are permanent;
- a protocol response with `success = false` is retryable.

HTTP retries `408`, `425`, `429`, and `5xx`.
Other non-success HTTP statuses are permanent.

gRPC retries transient transport statuses.
Invalid request, authentication, and other permanent client statuses stop retries.

A rejected response may include `retry_after_s`.
The task clamps the value to zero through one hour.
The hold uses a monotonic deadline.
Taskvisor backoff still applies after the failed attempt.

Retryable failures become `TaskError::Fail`.
Permanent failures become `TaskError::Fatal`.

## Generated task

`sync` returns an embedded task with this policy:

| Setting         | Value                                                    |
|-----------------|----------------------------------------------------------|
| Name and slot   | `solti-discover-sync`                                    |
| Workload        | `TaskWorkload::Embedded`                                 |
| Revision        | `DiscoverConfig::task_revision`                          |
| Restart         | Periodic with `delay_ms`                                 |
| Admission       | `AdmissionPolicy::Replace`                               |
| Backoff         | Configured value or the derived default                  |
| Attempt timeout | Jitter, retry hold, connect timeout, and request timeout |

The first attempt waits for startup jitter below `delay_ms`.
Later attempts do not repeat startup jitter.

The attempt timeout includes:

- one discovery interval;
- the maximum one-hour server hold;
- connect timeout;
- request timeout;
- one second of overhead.

## Uptime

The application owns the uptime epoch.
`MonotonicUptime` starts at construction and ignores wall-clock changes.

Create it at the agent lifecycle boundary:

```rust
use solti_discover::{MonotonicUptime, UptimeSource};

let uptime = MonotonicUptime::new();
let elapsed = uptime.uptime_seconds();
println!("{elapsed}");
```

Implement `UptimeSource` or pass a closure when another epoch is required.
Every attempt reads the source again.

## Metrics

`DiscoverMetricsBackend` receives:

| Hook             | Value                                  |
|------------------|----------------------------------------|
| `record_attempt` | One call before each transport request |
| `record_success` | Request duration in milliseconds       |
| `record_failure` | Duration and bounded failure reason    |
| `record_hold`    | Clamped server hold in seconds         |

`NoOpDiscoverMetrics` is the default.
`DiscoverFailReason` keeps failure labels bounded.

## Specific behavior

- Every attempt sends a fresh Unix timestamp and uptime value.
- HTTP and gRPC clients are reused across task attempts.
- HTTP redirects are disabled to avoid forwarding credentials to another host.
- Successful HTTP response bodies are limited to 64 KiB.
- Non-success HTTP responses read at most 1 KiB of body data.
- Control-plane rejection text remains untrusted diagnostic text.
- Linux OS metadata uses `PRETTY_NAME` from `os-release` when available.
- The generated task captures its config and uptime source.
- The crate does not persist registration state.

## Errors

| Error             | Cause                                               | Retryability |
|-------------------|-----------------------------------------------------|--------------|
| `InvalidConfig`   | Invalid endpoint, duration, auth data, or TLS setup | Permanent    |
| `SpecBuild`       | Embedded task manifest could not be built           | Permanent    |
| `GrpcTransport`   | gRPC connection failed                              | Retryable    |
| `GrpcStatus`      | Control plane returned a gRPC status                | Code-based   |
| `HttpRequest`     | HTTP connection, TLS, timeout, or body failure      | Retryable    |
| `HttpStatus`      | Control plane returned a non-success HTTP status    | Code-based   |
| `InvalidResponse` | HTTP response could not be decoded safely           | Retryable    |
| `Rejected`        | Protocol response contained `success = false`       | Retryable    |
| `AuthFailed`      | HTTP or gRPC authentication was rejected            | Permanent    |

Transport-specific variants are available only with their transport feature.
`DiscoverError` is non-exhaustive.
Keep a wildcard arm when matching it.

## Protocol

See [Sync Protocol v1](sync_v1.md) for the wire fields and transport mapping.
The HTTP endpoint is always `POST /api/v1/discovery/sync`.
Generated protobuf types remain internal.
