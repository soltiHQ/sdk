# Discovery Protocol Contract

The current discovery protocol identity is `v1`.

This document describes the discovery v1 wire contract.
It also describes the client behavior implemented by `solti-discover`.

Discovery v1 has two transport bindings:

| Transport | Protocol identity                                      |
|-----------|--------------------------------------------------------|
| gRPC      | Package `solti.discover.v1`, service `DiscoverService` |
| HTTP      | `POST /api/v1/discovery/sync`                          |

`SyncRequest.api_version` does not select the discovery protocol.
It identifies the API exposed by the agent.

Discovery protocol versions and Task API versions are independent.
Discovery v1 can advertise agents that expose different Task API majors.

The control plane selects the Task API adapter by `endpoint_type` and `api_version`.
An unsupported pair cannot be used to call the agent.

## Endpoint roles

Discovery uses two independent endpoints.

```text
Control plane ── calls ──► AgentEndpoint
                           API exposed by the agent

Discovery task ── syncs ──► ControlPlaneEndpoint
                             outbound discovery endpoint
```

An HTTP agent API can use gRPC discovery.
A gRPC agent API can use HTTP discovery.

## Attempt flow

`sync` builds one embedded periodic task.
Taskvisor owns its attempt lifecycle.

```text
Taskvisor starts an attempt
            ├── first attempt ──► startup jitter below delay_ms
            ├── wait for any remaining server hold
            ├── stamp ts and uptime_seconds
            └── send SyncRequest
                       ├── success ──► Ok
                       │               └── delay_ms ──► next attempt
                       ├── retryable failure ──► TaskError::Fail
                       │                            └── Taskvisor backoff
                       └── permanent failure ──► TaskError::Fatal ──► stop

cancellation during an awaited step ──► TaskError::Canceled ──► stop
```

Startup jitter runs once for each constructed task.
Its value is in the range `0..delay_ms`.

`delay_ms` applies after a successful attempt.
Retryable failures use the configured Taskvisor backoff.

Every sleep and transport request observes task cancellation.

## Request construction

`sync` captures `DiscoverConfig` when it constructs the task.
It builds one base request from that snapshot.

Every attempt clones the base request.
Only `ts` and `uptime_seconds` are refreshed.

Apply a new embedded task when captured discovery settings change.
Use a new `task_revision` for the changed runtime intent.

The request contract is defined in
`proto/solti/discover/v1/discovery.proto`.

| Field                  | Protobuf type        | Value produced by the SDK                          |
|------------------------|----------------------|----------------------------------------------------|
| `id`                   | `string`             | Caller-provided `AgentId`                          |
| `name`                 | `string`             | Trimmed agent display name                         |
| `endpoint`             | `string`             | Address advertised through `AgentEndpoint`         |
| `uptime_seconds`       | `int64`              | Whole seconds from the supplied `UptimeSource`     |
| `os`                   | `string`             | OS description                                     |
| `arch`                 | `string`             | `std::env::consts::ARCH`                           |
| `platform`             | `string`             | `std::env::consts::OS`                             |
| `ts`                   | `int64`              | Current Unix timestamp in seconds                  |
| `metadata`             | `map<string,string>` | Captured caller-provided metadata                  |
| `endpoint_type`        | `EndpointType`       | Transport exposed by the advertised agent endpoint |
| `api_version`          | `int32`              | Version of the API exposed by the agent            |
| `heartbeat_interval_s` | `int32`              | `ceil(delay_ms / 1000)`                            |
| `capabilities`         | `AgentCapabilities`  | Captured runner capability snapshot                |

The caller owns `AgentId` assignment and uniqueness.
The SDK validates its format.

`AgentEndpoint` accepts `api_version` values from `1` through `i32::MAX`.

`AgentEndpointType` can produce only `ENDPOINT_TYPE_GRPC` or `ENDPOINT_TYPE_HTTP`.
It cannot produce `ENDPOINT_TYPE_UNSPECIFIED`.

On Linux, `os` uses `PRETTY_NAME` from `/etc/os-release`.
It then tries `/usr/lib/os-release`.
It falls back to `std::env::consts::OS`.

On other platforms, `os` equals `std::env::consts::OS`.

The application owns the uptime epoch.
`MonotonicUptime` starts its epoch when it is constructed.
A custom `UptimeSource` can use another lifecycle boundary.

## Capabilities

Capability messages are defined in
`proto/solti/agent/v1/types.proto`.

`capabilities` is present in every request.
Its `runners` list may be empty.

`AgentCapabilities` preserves its supplied runner order.
`RunnerRouter::capabilities` supplies registration order.
That order is also routing priority.

Each `RunnerCapability` contains:

| Field       | Protobuf type           | Meaning                              |
|-------------|-------------------------|--------------------------------------|
| `name`      | `string`                | Unique runner name in the snapshot   |
| `labels`    | `map<string,string>`    | Static runner labels                 |
| `workloads` | `repeated WorkloadType` | Exact workload GVKs accepted by it   |

Runner names must be unique inside one capability snapshot.
Workload GVKs use canonical order inside each runner.

Each `WorkloadType` contains `api_version` and `kind`.
Routing matches both values exactly.

`Embedded` cannot be declared by a runner capability.
Embedded workloads bypass runner routing.

An empty `runners` list means that the agent advertises no routable runner.

## Response semantics

The response contract is:

| Field           | Protobuf type | Client behavior                                      |
|-----------------|---------------|------------------------------------------------------|
| `success`       | `bool`        | Accepts the sync when `true`                         |
| `reason`        | `string`      | Untrusted diagnostic text used when rejected         |
| `retry_after_s` | `int32`       | Server hold used only when rejected                  |

`success = true` completes the attempt.
The client ignores `reason` and `retry_after_s` in that response.

`success = false` creates `DiscoverError::Rejected`.
This error is retryable.

An empty rejection reason is replaced with a local diagnostic message.
The reason is never interpreted as machine-readable data.

`retry_after_s <= 0` means that no hold is requested.
A positive value creates a monotonic hold deadline.
The client clamps it to at most `3600` seconds.

```text
success = false with retry_after_s > 0
                   ├── hold deadline = now + clamped value
                   └── TaskError::Fail
                              └── Taskvisor backoff
                                         └── next attempt
                                                ├── wait until hold deadline
                                                │   when it is still active
                                                └── send next request
```

Taskvisor backoff and server hold are not added together.
The next request starts only after both constraints have elapsed.

## HTTP binding

The HTTP binding requires the `http` feature.

The client sends JSON with:

```text
POST {control-plane base path}/api/v1/discovery/sync
Content-Type: application/json
```

The endpoint must use `http` or `https`.
Queries and fragments are rejected.
An existing base path is preserved.

The JSON encoding follows protobuf JSON conventions.
Field names use lower camel case.
The `int64` values `ts` and `uptimeSeconds` are encoded as decimal strings.
`endpointType` is encoded as its symbolic protobuf name.
Fields containing protobuf default values are omitted.

One `reqwest::Client` is built with the task.
The client is reused across attempts.
Redirects are disabled.

Successful response bodies are limited to 64 KiB.
Non-success responses read at most 1 KiB of body data.

HTTP `401` and `403` become permanent authentication failures.
HTTP `408`, `425`, `429`, and `5xx` statuses are retryable.
Other non-success statuses are permanent.

Connection, timeout, body, and response decoding failures are retryable.

An optional bearer token uses the `Authorization` header.
The token requires `https` by default.
Plaintext HTTP with a token is rejected during adapter construction.
`allow_insecure_token_transport()` permits it only as an explicit development
or loopback opt-in and emits a warning.
Plaintext HTTP without a token remains valid.

HTTP `https` uses platform roots by default.
Custom roots and client identity require the `tls` feature.
Custom TLS also requires an `https` endpoint.

## gRPC binding

The gRPC binding requires the `grpc` feature.

The client calls:

```text
/solti.discover.v1.DiscoverService/Sync
```

The endpoint must use `http` or `https`.
The first sync attempt starts the connection.

A successful client is stored in `OnceCell`.
Later attempts reuse its channel.
A failed connection is not cached.

An optional bearer token uses `authorization` metadata.
The token requires `https` by default.
Plaintext gRPC with a token is rejected during adapter construction.
`allow_insecure_token_transport()` permits it only as an explicit development
or loopback opt-in and emits a warning.
Plaintext gRPC without a token remains valid.

gRPC `https` requires the `tls` feature.
It uses platform roots when custom TLS is absent.
Custom TLS also requires an `https` endpoint.

`Unauthenticated` and `PermissionDenied` become permanent authentication failures.

These gRPC statuses are permanent:

- `InvalidArgument`
- `NotFound`
- `AlreadyExists`
- `Unauthenticated`
- `PermissionDenied`
- `FailedPrecondition`
- `OutOfRange`
- `Unimplemented`

Other gRPC statuses are retryable.
Connection failures are retryable.

## Failure classification

Configuration and task construction failures are returned by `sync`.
They occur before the embedded task is available.

Runtime discovery failures are converted into Taskvisor errors.

HTTP client construction can also return `HttpRequest` from `sync`.
No automatic retry exists before a task has been returned.

| Runtime discovery failure | Retryability | Taskvisor result                        |
|---------------------------|--------------|-----------------------------------------|
| `AuthFailed`              | Permanent    | `TaskError::Fatal`                      |
| `Rejected`                | Retryable    | `TaskError::Fail`                       |
| `HttpRequest`             | Retryable    | `TaskError::Fail`                       |
| `InvalidResponse`         | Retryable    | `TaskError::Fail`                       |
| `HttpStatus`              | Code-based   | `TaskError::Fatal` or `TaskError::Fail` |
| `GrpcTransport`           | Retryable    | `TaskError::Fail`                       |
| `GrpcStatus`              | Code-based   | `TaskError::Fatal` or `TaskError::Fail` |

## Generated task

`sync` returns a `TaskManifest` and an embedded `TaskRef`.

| Setting                 | Value                                   |
|-------------------------|-----------------------------------------|
| Name and slot           | `solti-discover-sync`                   |
| Workload                | `TaskWorkload::Embedded`                |
| Implementation revision | Configured `task_revision`              |
| Restart                 | `Always` with interval `delay_ms`       |
| Admission               | `AdmissionPolicy::Replace`              |
| Backoff                 | Configured value or the derived default |

The derived backoff is:

| Setting | Value                          |
|---------|--------------------------------|
| Jitter  | `Equal`                        |
| First   | `max(delay_ms / 2, 1)`         |
| Maximum | `delay_ms.saturating_mul(3)`   |
| Factor  | `2.0`                          |

`delay_ms` must be greater than zero.
The derived heartbeat interval must fit `i32`.

The default connect timeout is 5 seconds.
The default request timeout is 30 seconds.
Both configured timeout values must be greater than zero.

The attempt timeout is built from:

```text
delay_ms
+ 3_600_000 ms maximum server hold
+ connect_timeout_ms
+ request_timeout_ms
+ 1_000 ms overhead
```

The additions saturate at `u64::MAX`.
Taskvisor retries an attempt timeout through the configured failure backoff.
