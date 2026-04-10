# solti-api

Dual-transport API layer exposing task operations over gRPC and HTTP.

Both transports delegate to an `ApiHandler` trait, decoupling wire format from business logic.

## Architecture
```text
 Control Plane / Client
     │
     ├──► gRPC (feature = "grpc")
     │        └──► SoltiApiService<H>
     │                  │
     ├──► HTTP (feature = "http")
     │        └──► HttpApi<H> (axum Router)
     │                  │
     ▼                  ▼
 ApiHandler trait (transport-agnostic)
     │
     ▼
 SupervisorApiAdapter
     │
     ▼
 solti_core::SupervisorApi
```

## API surface

| Operation | gRPC RPC | HTTP Endpoint |
|-----------|----------|---------------|
| Submit task | `SubmitTask` | `POST /api/v1/tasks` |
| Get task | `GetTaskStatus` | `GET /api/v1/tasks/{id}` |
| List tasks | `ListTasks` | `GET /api/v1/tasks` |
| List runs | `ListTaskRuns` | `GET /api/v1/tasks/{id}/runs` |
| Cancel task | `CancelTask` | `POST /api/v1/tasks/{id}/cancel` |
| Delete task | `DeleteTask` | `DELETE /api/v1/tasks/{id}` |

## Key types

| Type | Role |
|------|------|
| `ApiHandler` | Transport-agnostic trait with 6 operations |
| `SupervisorApiAdapter` | Default adapter bridging to `SupervisorApi` |
| `ApiError` | Unified error mapped to gRPC Status / HTTP JSON |
| `SoltiApiService<H>` | gRPC server impl (feature `grpc`) |
| `HttpApi<H>` | axum router builder (feature `http`) |

## Protobuf contract

Defined in `proto/solti/v1/`:
- `api.proto` - service definition with 6 RPCs and request/response messages
- `types.proto` - shared types: `TaskStatus`, `CreateSpec`, `TaskInfo`, `TaskRunInfo`, policies

The proto carries `go_package` targeting `github.com/soltiHQ/control-plane/api/gen/v1` - the Go control-plane is the primary consumer.

## Error model

| Variant | gRPC Status | HTTP Status |
|---------|-------------|-------------|
| `InvalidRequest` | `INVALID_ARGUMENT` | `400 Bad Request` |
| `TaskNotFound` | `NOT_FOUND` | `404 Not Found` |
| `Internal` | `INTERNAL` | `500 Internal Server Error` |
| `Core` | `INTERNAL` | `500 Internal Server Error` |

## Feature flags

| Flag | Enables | Dependencies |
|------|---------|--------------|
| `grpc` | `SoltiApiService`, `SoltiApiServer`, proto codegen, `convert` | `tonic`, `prost` |
| `http` | `HttpApi`, axum router | `axum`, `serde_json` |

Neither feature is enabled by default.

## Notes

- `ApiHandler` uses `async_trait` for object safety (`Send + Sync + 'static`).
- gRPC path validates input via `convert_create_spec` with slot, timeout, backoff bounds checks.
- HTTP path accepts `TaskSpec` directly from JSON (validation delegated to model layer).
- Both transports re-exported: `solti_api::tonic`, `solti_api::axum` for version pinning.
- `convert.rs` has 60+ unit tests covering all task kinds, policies, and rejection cases.
