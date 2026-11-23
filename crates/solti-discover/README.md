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
        solti_model::AgentCapabilities::default(),
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
- advertises each workload type's schema fingerprint, and sends the schemas the
  control plane requests in follow-up requests after the heartbeat, within the
  encoded request budget;
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
| `capabilities`                | `AgentCapabilities` snapshot                  | Runner capabilities and schema fingerprints sent with every sync; schemas on request |
| `with_token`                  | `Token`                                       | Bearer authentication                     |
| `allow_insecure_token_transport` | No value                                   | Explicit development/loopback opt-in      |
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
| `backoff`                | Equal jitter, half interval to three intervals, factor 2 |
| `connect_timeout_ms`     | 5 seconds                                                |
| `request_timeout_ms`     | 30 seconds                                               |
| Metrics                  | `NoOpDiscoverMetrics`                                    |
| Bearer token             | None                                                     |
| Insecure token transport | Disabled                                                 |
| Custom TLS               | None                                                     |

`delay_ms`, `connect_timeout_ms`, and `request_timeout_ms` use milliseconds.
The heartbeat interval sent on the wire uses seconds rounded up.

`task_revision` identifies the complete runtime intent captured by the embedded task.
Change it when any captured discovery setting changes, including credentials,
TLS material, or the metrics backend. Also change it when the `UptimeSource`
or its epoch changes.
This lets `solti-core` reconcile an otherwise identical embedded workload.

## Capabilities

Capabilities are a required `DiscoverConfig::builder` input. For a running
`SupervisorApi`, use the exact immutable router snapshot returned by
`SupervisorApi::runner_capabilities()`:

```text
registered runners
       ▼
SupervisorApi::runner_capabilities()
       ▼
DiscoverConfig::builder(..., capabilities, ...)
       ▼
discovery SyncRequest
```

Each runner contributes:

- its unique name;
- static routing labels;
- exact workload `apiVersion` and `kind` values, each with its schema fingerprint.

Runner order preserves routing priority.
Workload GVKs use canonical order.
Embedded workloads are absent because they bypass runner routing.

An agent with no routed runners passes `AgentCapabilities::default()` explicitly.

The snapshot must keep the heartbeat within the request budget of 256 KiB
minus 1 KiB. `sync` returns `DiscoverError::InvalidConfig` for a snapshot that
does not.

## Workload schemas

Every heartbeat advertises the fingerprint of each declared workload type:
the SHA-256 of the RFC 8785 canonical JSON of its schema. The control plane
answers with `want_schemas`, the fingerprints it lacks. The task then sends
exactly those canonical bytes in follow-up requests of the same attempt:

- each follow-up carries the requested schemas, in sorted order, as many as fit
  the encoded request budget of the transport in use;
- at most six follow-ups run per attempt;
- a schema is sent once per attempt;
- a schema the control plane still requests right after it was sent is resent
  at most every five minutes, while it stays requested;
- a schema the control plane requests again after it stopped requesting it,
  because it lost it, goes out in the next follow-up, without waiting.

The heartbeat itself never carries a schema, so a schema request refused for
its size, or too slow for the link, can never fail the heartbeat. Its response
restates `want_schemas`, so the follow-ups start right after it.

The budget counts the bytes on the wire. HTTP carries schema bytes as base64,
so a follow-up holds about two 64 KiB schemas over HTTP and about three over
gRPC.

A failed follow-up is logged at WARN and does not fail the attempt, whatever
the error: the next attempt's follow-ups send what is still requested.

The control plane refuses a schema it cannot compile silently: it stops
requesting it. The agent cannot see that. Look at the control plane's workload
catalog, where the schema is `refused`, and at placement, which reports
`schema_incompatible` with the reason.

| Log event                              | Level | Meaning |
|----------------------------------------|-------|---------|
| `discovery.workload_types`             | INFO  | Once per task: each `apiVersion/kind` with its fingerprint |
| `discovery.schemas_sent`               | INFO  | An accepted follow-up: fingerprints and encoded request size |
| `discovery.schema_still_requested`     | WARN  | Requested again by the response to the follow-up that carried it; once per fingerprint for each discovery task until a response stops requesting it |
| `discovery.schema_unknown_request`     | DEBUG | A requested fingerprint the agent does not declare; ignored |
| `discovery.schema_follow_up_failed`    | WARN  | A follow-up failed; the attempt still succeeds |
| `discovery.schema_over_request_budget` | WARN  | A schema cannot fit in a follow-up even alone and is never sent |

[`CONTRACT.md`](CONTRACT.md#schema-exchange) gives the exact rules.

## Authentication and TLS

`with_token` sends the same bearer token with every sync:

- HTTP uses the `Authorization` header;
- gRPC uses `authorization` metadata.

The selected adapter validates the encoded value before the task starts.
A token requires an `https` control-plane endpoint by default.
`sync` returns `DiscoverError::InvalidConfig` without returning a task when a
token is combined with plaintext HTTP or gRPC.

Plaintext discovery without a token remains supported.
`allow_insecure_token_transport()` is an explicit escape hatch for controlled
development or loopback endpoints.
It permits the plaintext credential and emits a warning.
Do not use this opt-in for production credentials.

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
| Attempt timeout | Jitter, retry hold, and seven requests                   |

The first attempt waits for startup jitter below `delay_ms`.
Later attempts do not repeat startup jitter.

The attempt timeout includes:

- one discovery interval;
- the maximum one-hour server hold;
- connect timeout plus request timeout for the heartbeat and each of up to six
  schema follow-ups;
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
Values above `i64::MAX` cannot be represented by discovery v1 and fail that
attempt permanently before a transport request is sent. This local validation
failure does not record a transport attempt or transport failure metric.

## Metrics

`DiscoverMetricsBackend` receives:

| Hook             | Value                                  |
|------------------|----------------------------------------|
| `record_attempt` | One call after wire stamping and before each transport request |
| `record_success` | Request duration in milliseconds       |
| `record_failure` | Duration and bounded failure reason    |
| `record_hold`    | Clamped server hold in seconds         |

Schema follow-ups are transport requests too, so each records its own attempt
and outcome.
`NoOpDiscoverMetrics` is the default.
`DiscoverFailReason` keeps failure labels bounded.
`build` installs one sticky panic boundary around the selected backend. After
its first callback panic, later task attempts drop metrics updates without
invoking that backend again.
Callbacks that already entered the boundary concurrently may still finish or
panic. The boundary does not serialize healthy metrics updates.

## Specific behavior

- Every request sends a fresh Unix timestamp and uptime value.
- Every request stays within the encoded request budget of 256 KiB minus 1 KiB.
- The heartbeat never carries a schema.
- A schema is sent only in a follow-up, only when the control plane requests it,
  as its canonical bytes.
- HTTP and gRPC clients are reused across task attempts.
- HTTP redirects are disabled to avoid forwarding credentials to another host.
- Bearer tokens require TLS unless insecure token transport is explicitly enabled.
- Successful HTTP response bodies are limited to 64 KiB.
- Non-success HTTP responses read at most 1 KiB of body data.
- Control-plane rejection text remains untrusted diagnostic text.
- Linux OS metadata uses `PRETTY_NAME` from `os-release` when available.
- The generated task captures its config and uptime source.
- The crate does not persist registration state.

## Errors

| Error             | Cause                                               | Retryability |
|-------------------|-----------------------------------------------------|--------------|
| `InvalidConfig`   | Invalid endpoint, duration, auth data, or TLS setup; a heartbeat over the request budget | Permanent    |
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

## Examples

### Internal examples

These examples exercise only the discovery client, generated task, and direct transport contracts.
They do not run the agent API, persist registration state, or submit work to `solti-core`.
Each example starts with a text flow diagram, then explains its inputs, wire interaction, and result.

Start with a self-contained HTTP heartbeat:

```bash
cargo run -p solti-discover --example http_sync --features http
```

| Example                                     | Features       | What it shows                                                         |
|---------------------------------------------|----------------|-----------------------------------------------------------------------|
| [http_sync.rs](examples/http_sync.rs)       | `http`         | Full HTTP requests, capabilities with fingerprints, a requested schema sent in a follow-up, authentication, uptime, and metrics. |
| [grpc_sync.rs](examples/grpc_sync.rs)       | `grpc`         | gRPC task construction, lazy connection, and optional real sync.      |
| [retryability.rs](examples/retryability.rs) | `http`, `grpc` | Retryable and permanent HTTP, gRPC, and protocol failures.            |

Run the remaining examples explicitly:

```bash
cargo run -p solti-discover --example grpc_sync --features grpc
cargo run -p solti-discover --example retryability --features http,grpc
```

The gRPC example performs no network operation by default.
Pass `--send` only when `SOLTI_DISCOVERY_GRPC_ENDPOINT` points to a compatible control plane.

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine discovery with runner registration, reconciliation, supervision, and the agent API.

## Contracts

| Contract | Source                                                         |
|----------|----------------------------------------------------------------|
| Behavior | [Discovery protocol contract](CONTRACT.md)                     |
| Wire     | [`solti.discover.v1`](proto/solti/discover/v1/discovery.proto) |

Protobuf defines the gRPC service and shared request and response messages.
`CONTRACT.md` defines the HTTP binding and behavior shared by both transports.

The authoritative protobuf tree lives in
[`soltiHQ/proto`](https://github.com/soltiHQ/proto).
Release and CI tooling vendor a pinned revision into `proto/`.
The crate build script generates Rust and protobuf-JSON bindings from that
vendored contract into Cargo's `OUT_DIR`. Generated bindings are build artifacts
and are never committed. The build uses a vendored `protoc` binary and does not
require a system installation.

The HTTP endpoint is always `POST /api/v1/discovery/sync`.
Generated protobuf types remain internal.
