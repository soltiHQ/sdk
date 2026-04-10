# solti-discover

Periodic heartbeat that registers an agent with the control plane and reports liveness, platform telemetry, and capabilities.

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

## Sync flow
```text
 Agent                          Control Plane
     │                                  │
     ├──► SyncRequest (gRPC / HTTP) ──► │
     │◄── SyncResponse { success } ◄────│
     │         ... delay_ms ...         │
     ├──► SyncRequest ────────────────► │
     │        (backoff on failure)      │
```

Each cycle stamps the base request with fresh `ts` and `uptime_seconds`, then sends via the configured transport.

## Key types

| Type                 | Role                                                        |
|----------------------|-------------------------------------------------------------|
| `DiscoverConfig`     | Agent identity, endpoint, transport, interval, capabilities |
| `DiscoveryTransport` | Selects gRPC or HTTP path                                   |
| `DiscoverError`      | Transport, parse, and rejection failures                    |
| `sync()`             | Factory - returns `(TaskRef, TaskSpec)` for the supervisor  |
| `SyncRequest`        | Protobuf message sent each cycle                            |
| `SyncResponse`       | Protobuf ack from control plane                             |

## Protobuf contract

Defined in `proto/v1/sync.proto` (package `solti.discover.v1`).

`SyncRequest` carries:
- agent identity (`id`, `name`, `endpoint`)
- platform telemetry (`os`, `arch`, `platform`)
- timing (`ts`, `uptime_seconds`, `heartbeat_interval_s`)
- capabilities (`task_runs`, `task_delete`, `cancel`)
- transport type and API version

`SyncResponse` returns `bool success`.

## Transport details

**gRPC** - uses `tonic` generated `DiscoverServiceClient`. 
Connects to `control_plane_endpoint` and calls `Sync` RPC. 
Channel is created once and reused across heartbeat cycles.

**HTTP** - uses `reqwest::Client` with JSON serialization. 
Posts to `{control_plane_endpoint}/api/v1/discovery/sync`. 
Client is reused across cycles (connection pooling).

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
