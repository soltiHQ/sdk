# Sync Protocol v1

The discovery protocol version is `v1`.

`SyncRequest.api_version` is the version of the API exposed by the agent. It is
data inside the discovery request and does not select the discovery protocol.

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

Each cycle stamps the base request with fresh `ts` and `uptime_seconds`, then sends via the configured transport. The host supplies the monotonic uptime source and explicitly owns its epoch.

## Protobuf contract

Defined in `proto/solti/discover/v1/discovery.proto` (package `solti.discover.v1`).

### SyncRequest fields

| Field                  | Type               | Description                                |
|------------------------|--------------------|--------------------------------------------|
| `id`                   | string             | Unique agent identifier                    |
| `name`                 | string             | Agent name                                 |
| `endpoint`             | string             | Where control plane can reach this agent   |
| `platform`             | string             | OS name (`linux`, `macos`, `windows`)      |
| `arch`                 | string             | CPU architecture (`x86_64`, `aarch64`)     |
| `os`                   | string             | OS distribution info (Linux `PRETTY_NAME`) |
| `metadata`             | map<string,string> | User-provided key-value pairs              |
| `ts`                   | int64              | Unix timestamp (seconds)                   |
| `uptime_seconds`       | int64              | Elapsed time from the host-owned agent-composition epoch |
| `endpoint_type`        | EndpointType       | `UNSPECIFIED = 0`, `GRPC = 1`, `HTTP = 2`  |
| `api_version`          | int32              | Agent API protocol version (`1` = v1)      |
| `heartbeat_interval_s` | int32              | Agent-reported sync interval, rounded up   |
| `capabilities`         | AgentCapabilities  | Registered runners and workload GVKs       |

### Capabilities

`AgentCapabilities.runners` keeps runner registration order. Each
`RunnerCapability` contains:

| Field       | Type               | Description                              |
|-------------|--------------------|------------------------------------------|
| `name`      | string             | Unique runner name                       |
| `labels`    | map<string,string> | Labels matched by Task runner selectors  |
| `workloads` | repeated WorkloadType | Exact workload GVKs handled by the runner |

Each `WorkloadType` contains `api_version` and `kind`. Embedded workloads are
not advertised because they do not use a runner. An empty `runners` list means
the agent has no routable runner.

### SyncResponse fields

| Field           | Type   | Description                                             |
|-----------------|--------|---------------------------------------------------------|
| `success`       | bool   | Control plane ack                                       |
| `reason`        | string | Human-readable reason when `success = false` (optional) |
| `retry_after_s` | int32  | Server-suggested backoff (seconds); `0` = unspecified   |

## Transport details

**gRPC** - `tonic` generated `DiscoverServiceClient`.
Connects to `control_plane_endpoint`, calls `Sync` RPC.
Channel created lazily via `OnceCell`, reused across cycles.

**HTTP** - `reqwest::Client` with JSON serialization.
Posts to `{control_plane_endpoint}/api/v1/discovery/sync`.
Client reused across cycles (connection pooling).
