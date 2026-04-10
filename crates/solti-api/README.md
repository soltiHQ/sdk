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

## Versioning

`solti-api` exports `API_VERSION: u32` - the current protocol version.

Binary passes it to `solti_discover::DiscoverConfig::api_version`, which reports it to the control-plane via `SyncRequest`. One binary = one API version.

```rust
use solti_api::API_VERSION;

let config = DiscoverConfig {
    api_version: API_VERSION,
    // ...
};
```

Bump rules:
- New field in existing message - no bump (proto3 backwards compatible)
- New RPC - no bump (control-plane does not call unsupported RPCs)
- Removed/renamed field, changed semantics - bump
- New proto package (`solti.v2`) - bump

Internal crate compatibility is handled by cargo semver.

Per-version API surface is documented in separate files: [api_v1.md](api_v1.md).

## Key types

| Type | Role |
|------|------|
| `ApiHandler` | Transport-agnostic trait with 6 operations |
| `SupervisorApiAdapter` | Default adapter bridging to `SupervisorApi` |
| `ApiError` | Unified error mapped to gRPC Status / HTTP JSON |
| `SoltiApiService<H>` | gRPC server impl (feature `grpc`) |
| `HttpApi<H>` | axum router builder (feature `http`) |
| `API_VERSION` | Protocol version constant reported via discover |

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
- Proto contract defined in `proto/solti/v1/` (`api.proto`, `types.proto`).
