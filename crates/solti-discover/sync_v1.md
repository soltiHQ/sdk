# Sync Protocol v1

`API_VERSION = 1`

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

## Protobuf contract

Defined in `proto/v1/sync.proto` (package `solti.discover.v1`).

### SyncRequest fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique agent identifier |
| `name` | string | Agent name |
| `endpoint` | string | Where control plane can reach this agent |
| `platform` | string | OS name (`linux`, `macos`, `windows`) |
| `arch` | string | CPU architecture (`x86_64`, `aarch64`) |
| `os` | string | OS distribution info (Linux `PRETTY_NAME`) |
| `metadata` | map<string,string> | User-provided key-value pairs |
| `ts` | int64 | Unix timestamp (seconds) |
| `uptime_seconds` | int64 | Agent process uptime |
| `endpoint_type` | EndpointType | `GRPC = 0`, `HTTP = 1` |
| `api_version` | APIVersion | `V1 = 1` |
| `heartbeat_interval_s` | int32 | Agent-reported sync interval |
| `capabilities` | repeated string | Reserved, sent empty in v1 |

### SyncResponse fields

| Field | Type | Description |
|-------|------|-------------|
| `success` | bool | Control plane ack |

## Transport details

**gRPC** - `tonic` generated `DiscoverServiceClient`.
Connects to `control_plane_endpoint`, calls `Sync` RPC.
Channel created lazily via `OnceCell`, reused across cycles.

**HTTP** - `reqwest::Client` with JSON serialization.
Posts to `{control_plane_endpoint}/api/v1/discovery/sync`.
Client reused across cycles (connection pooling).
