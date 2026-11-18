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
            └── send the heartbeat SyncRequest, never with a schema
                       ├── success ──► requested schemas left to send?
                       │                 ├── yes ──► follow-up SyncRequest, at most 6 per attempt
                       │                 │             ├── success ──► ask again
                       │                 │             └── failure ──► WARN, Ok
                       │                 └── no ──► Ok
                       │                             └── delay_ms ──► next attempt
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

The heartbeat decides the outcome of the attempt.
It never carries a schema, so schema delivery can never fail it: a schema
request refused for its size, or too slow for the link, fails only its
follow-up.
Follow-ups exist only for the [schema exchange](#schema-exchange).

## Request construction

`sync` captures `DiscoverConfig` when it constructs the task.
It builds one base request from that snapshot.

Every request, heartbeat or follow-up, clones the base request.
Only `ts`, `uptime_seconds` and, in a follow-up, `schemas` change.
`ts` and `uptime_seconds` are converted to the signed 64-bit discovery v1 wire
fields with checked conversions. For the heartbeat, an out-of-range uptime is
`InvalidConfig`, does not reach the transport, does not record transport
attempt or failure metrics, and terminates the task attempt as
`TaskError::Fatal`. For a follow-up, it ends the attempt as a failed follow-up.

Apply a new embedded task when captured discovery settings change.
Use a new `task_revision` for the changed runtime intent.
This includes credentials, TLS material, the metrics backend, and the supplied
`UptimeSource` or its epoch. The current public interfaces do not expose a
stable identity for every opaque value. Revision ownership therefore remains
with the composition root.

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
| `schemas`              | `repeated WorkloadSchema` | Follow-ups only: requested schemas that fit the request budget; always empty in a heartbeat |

The caller owns `AgentId` assignment and uniqueness.
The SDK validates its format.

Podium caps one encoded request at 256 KiB, the HTTP body or the gRPC message.
The SDK keeps every request within a budget below that cap.
`sync` refuses a capability snapshot whose heartbeat is over the budget;
see [Schema exchange](#schema-exchange).

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

Each `WorkloadType` contains `api_version`, `kind` and `schema_fingerprint`.
Routing matches `api_version` and `kind` exactly.
`schema_fingerprint` is the lowercase hex SHA-256 of the RFC 8785 canonical
JSON of the type's schema: 64 characters, never empty.
Two types with the same schema share one fingerprint.
A GVK declared by two runners appears under each of them, with each runner's
own fingerprint; the first runner that matches routes the task.

`Embedded` cannot be declared by a runner capability.
Embedded workloads bypass runner routing.

An empty `runners` list means that the agent advertises no routable runner.

## Response semantics

The response contract is:

| Field           | Protobuf type     | Client behavior                                  |
|-----------------|-------------------|--------------------------------------------------|
| `success`       | `bool`            | Accepts the sync when `true`                     |
| `reason`        | `string`          | Untrusted diagnostic text used when rejected     |
| `retry_after_s` | `int32`           | Server hold used only when rejected              |
| `want_schemas`  | `repeated string` | Fingerprints of the schemas to send, when accepted |

`success = true` accepts the request.
The client ignores `reason` and `retry_after_s` in that response.
It reads `want_schemas` for the [schema exchange](#schema-exchange).

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

## Schema exchange

Each heartbeat advertises the schema fingerprint of every declared workload
type, and never carries a schema itself. Only follow-ups carry schemas, and a
follow-up carries one only when the control plane asked for its fingerprint in
`want_schemas`, and only for a fingerprint the same request declares.

| Constant                      | Value       | Meaning                                         |
|-------------------------------|-------------|-------------------------------------------------|
| `MAX_SYNC_REQUEST_BYTES`      | 262 144     | Podium's cap on one encoded request             |
| `SYNC_REQUEST_HEADROOM_BYTES` | 1 024       | Bytes kept free below that cap                  |
| Request budget                | 261 120     | The cap minus the headroom                      |
| `MAX_SCHEMA_FOLLOW_UPS`       | 6           | Follow-up requests per attempt, after the heartbeat |
| `SCHEMA_RESEND_INTERVAL`      | 300 seconds | Least time between two sends of one schema      |

These constants are crate-private.

### Requested schemas

The `want_schemas` of each accepted response replaces the set of requested
schemas. Only fingerprints the agent declares count. Any other entry is logged
at DEBUG and ignored.

A requested schema is *due* when no accepted request has carried it yet, or
the last one did at least 300 seconds ago. It is *sendable* when it is due and
the current attempt has not sent it. An attempt sends each schema at most once.

The heartbeat's response restates `want_schemas`, so keeping the heartbeat
bare adds no round trip: the follow-ups start right after it.

### Packing a follow-up

Every follow-up carries sendable schemas:

- in sorted fingerprint order, the order of `want_schemas`;
- as `WorkloadSchema { fingerprint, schema }`, where `schema` is exactly the
  canonical JSON whose SHA-256 is `fingerprint`;
- while the encoded request stays within the request budget. The first schema
  that does not fit ends the packing and goes in a later follow-up.

The size is measured with the encoder of the transport in use:

| Transport | Encoded size                                                          |
|-----------|-----------------------------------------------------------------------|
| gRPC      | `prost::Message::encoded_len` of the request                          |
| HTTP      | Length of the JSON body, `serde_json::to_vec`; bytes are standard base64 |

A follow-up therefore carries about three 64 KiB schemas over gRPC and about
two over HTTP. When the heartbeat learns that five schemas of exactly 65 536
bytes are requested, they go out in two follow-ups over gRPC (three, then two)
and in three over HTTP (two, two, then one).

A follow-up is the heartbeat's request plus its schemas, so a schema that
cannot fit beside the bare heartbeat can never be sent. `sync` logs
`discovery.schema_over_request_budget` at WARN for each such schema, and
packing skips it, so that it holds back no other schema.

`sync` returns `DiscoverError::InvalidConfig` when the heartbeat encodes over
the request budget. It measures that heartbeat with the largest `ts` and
`uptime_seconds` the wire carries, so no later heartbeat is larger.

### Follow-ups

After the accepted heartbeat, and after each accepted follow-up, the attempt
sends a follow-up at once while a requested schema is sendable, up to six
follow-ups. A follow-up in which no sendable schema fits is not sent. Each
follow-up gets a fresh `ts` and `uptime_seconds`, and records the same metrics
as the heartbeat.

A failed follow-up is logged at WARN as `discovery.schema_follow_up_failed`,
and the attempt returns `Ok`, because the heartbeat already succeeded. That
holds for every failure, a permanent one such as HTTP 413 included. The
schemas it carried are still due, so the next attempt's follow-ups send them
again, after its bare heartbeat. A rejection with `retry_after_s` still sets
the server hold.

### What the agent can and cannot see

| Situation                                                           | Agent behavior |
|---------------------------------------------------------------------|----------------|
| The response to a follow-up still asks for a schema that follow-up carried | WARN `discovery.schema_still_requested`, once per fingerprint for the task, with its workload types and size; the schema is resent at most every 300 seconds |
| The response asks for a fingerprint the agent does not declare      | DEBUG `discovery.schema_unknown_request`; ignored |
| The control plane refuses a schema                                  | Podium stops asking for it, which the agent cannot tell apart from success. Podium's catalog shows it as `refused`, and placement shows `schema_incompatible` with the reason |

A schema still requested right after it was sent means a store error or a
fingerprint mismatch on the control plane's side.

### Logs

| Event                                  | Level | When |
|----------------------------------------|-------|------|
| `discovery.workload_types`             | INFO  | Once per task, from `sync`: each `apiVersion/kind` with its schema fingerprint |
| `discovery.schemas_sent`               | INFO  | After each accepted follow-up: the fingerprints and the encoded request size |
| `discovery.schema_still_requested`     | WARN  | See the table above |
| `discovery.schema_unknown_request`     | DEBUG | See the table above |
| `discovery.schema_follow_up_failed`    | WARN  | A follow-up failed; the attempt still succeeds |
| `discovery.schema_over_request_budget` | WARN  | From `sync`, for each schema that cannot fit in a follow-up even alone |

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
The `bytes` value `schemas[].schema` is encoded as standard base64.
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
A capability snapshot whose heartbeat is over the request budget is one of them.

Runtime discovery failures of the heartbeat are converted into Taskvisor errors.
A failed follow-up is logged and not returned; see [Follow-ups](#follow-ups).

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
+ (1 + MAX_SCHEMA_FOLLOW_UPS) × (connect_timeout_ms + request_timeout_ms)
+ 1_000 ms overhead
```

`1 + MAX_SCHEMA_FOLLOW_UPS` is 7: the heartbeat and up to six follow-ups.

The additions saturate at `u64::MAX`.
Taskvisor retries an attempt timeout through the configured failure backoff.
