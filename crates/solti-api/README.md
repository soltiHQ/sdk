# solti-api

Dual-transport API layer exposing task operations over gRPC and HTTP.

Both transports delegate to an `ApiHandler` trait, decoupling wire format from business logic. Both speak the same proto contract defined in `proto/solti/v1/`.

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

| Flag   | Enables                                                   | Dependencies                        |
|--------|-----------------------------------------------------------|-------------------------------------|
| `grpc` | `SoltiApiService`, `SoltiApiServer`, proto codegen        | `tonic`, `tonic-prost`, `prost`     |
| `http` | `HttpApi`, axum router, proto-JSON serde                  | `axum`, `serde_json`, `prost`, `pbjson` |

Neither feature is enabled by default.

## Build

`build.rs` runs two codegen passes:
- `tonic_prost_build::configure()` - message types always, tonic server/client only under `grpc`.
- `pbjson_build` under `http` - attaches canonical proto-JSON `Serialize`/`Deserialize` to the same message types:

  ```rust
  pbjson_build::Builder::new()
      .register_descriptors(&descriptor_set)?
      .build(&[".solti.v1"])?;
  ```

  `".solti.v1"` is the proto package selector. If the `package` declaration in `.proto` changes, update this list - otherwise pbjson generates nothing and HTTP compile fails.

## Notes
- `ApiHandler` uses `async_trait` for object safety (`Send + Sync + 'static`).
- Both transports feed input through the same `convert_create_spec` validator.
- Re-exports: `solti_api::tonic`, `solti_api::axum` for version pinning.
- Proto contract in `proto/solti/v1/` (`api.proto`, `types.proto`).
