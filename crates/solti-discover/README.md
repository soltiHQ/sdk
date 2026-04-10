# solti-discover

Periodic heartbeat that registers an agent with the control plane and reports liveness and platform telemetry.

## Architecture
```text
 DiscoverConfig
     ▼
 sync(config) ──► (TaskRef, TaskSpec)
     ├──► gRPC transport (tonic Channel)
     │        └──► DiscoverService.Sync
     ├──► HTTP transport (reqwest Client)
     │        └──► POST /api/v1/discovery/sync
     ▼
 Control Plane
```

## Versioning

`DiscoverConfig` accepts `api_version: u32` from the binary. The value is passed into `SyncRequest.api_version` so the control-plane knows which API protocol the agent supports.

```rust
use solti_api::API_VERSION;

let config = DiscoverConfig {
    api_version: API_VERSION,
    // ...
};
```

The binary is the integration point - solti-discover does not depend on solti-api.

## Key types

| Type                 | Role                                                   |
|----------------------|--------------------------------------------------------|
| `DiscoverConfig`     | Agent identity, endpoint, transport, interval, version |
| `DiscoveryTransport` | Selects gRPC or HTTP path                              |
| `DiscoverError`      | Transport, parse, and rejection failures               |
| `sync()`             | Factory - returns `(TaskRef, TaskSpec)` for supervisor  |
| `SyncRequest`        | Protobuf message sent each cycle                       |
| `SyncResponse`       | Protobuf ack from control plane                        |

## Sync protocol

Per-version protocol details: [sync_v1.md](sync_v1.md).

## Error model

| Variant           | Cause                                      |
|-------------------|--------------------------------------------|
| `GrpcTransport`   | TCP / TLS / HTTP2 connection failure       |
| `GrpcStatus`      | Server returned non-OK gRPC status         |
| `HttpRequest`     | HTTP-level failure (connection, timeout)   |
| `InvalidResponse` | Response body failed JSON deserialization  |
| `Rejected`        | Control plane returned `success: false`    |

## Task policy

The sync task is created with:
- `RestartPolicy::periodic(delay_ms)` - runs on interval
- `BackoffPolicy` with equal jitter, `first_ms = delay_ms/2`, `max_ms = delay_ms*3`, factor 2.0
- `AdmissionPolicy::Replace` - new sync replaces a stale one
- Slot: `solti-discover-sync`

## Notes

- `SyncContext` is wrapped in `Arc` and shared into the async task closure.
- gRPC channel is lazily created via `OnceCell` and reused across cycles.
- `os_info()` reads `/etc/os-release` on Linux for distribution name, falls back to platform.
- Cancellation is cooperative via `tokio::select!` on the cancel token and the network future.
