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

Binary passes it to `solti_discover::DiscoverConfig::builder(... , API_VERSION)`, which reports it to the control-plane via `SyncRequest`. One binary = one API version.

```rust
use solti_api::API_VERSION;
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_model::AgentId;

let config = DiscoverConfig::builder(
    AgentId::new("agentd-001"),
    "agentd",
    "http://0.0.0.0:8085",
    "http://podium:8082",
    DiscoveryTransport::Http,
    10_000,
    API_VERSION,
)
.build()?;
```

Bump rules:
- New field in existing message - no bump (proto3 backwards compatible)
- New RPC - no bump (control-plane does not call unsupported RPCs)
- Removed/renamed field, changed semantics - bump
- New proto package (`solti.v2`) - bump

Internal crate compatibility is handled by cargo semver.

Per-version API surface is documented in separate files: [api_v1.md](api_v1.md).

## Key types

| Type                   | Role                                            |
|------------------------|-------------------------------------------------|
| `ApiHandler`           | Transport-agnostic trait with 6 operations      |
| `SupervisorApiAdapter` | Default adapter bridging to `SupervisorApi`     |
| `ApiError`             | Unified error mapped to gRPC Status / HTTP JSON |
| `SoltiApiService<H>`   | gRPC server impl (feature `grpc`)               |
| `HttpApi<H>`           | axum router builder (feature `http`)            |
| `API_VERSION`          | Protocol version constant reported via discover |

## Error model

| Variant          | gRPC Status                      | HTTP Status                 | `error` label (HTTP body)                  |
|------------------|----------------------------------|-----------------------------|--------------------------------------------|
| `InvalidRequest` | `INVALID_ARGUMENT`               | `400 Bad Request`           | `"InvalidRequest"`                         |
| `TaskNotFound`   | `NOT_FOUND`                      | `404 Not Found`             | `"TaskNotFound"`                           |
| `Internal`       | `INTERNAL`                       | `500 Internal Server Error` | `"Internal"`                               |
| `Core`           | derived from inner `CoreError`   | derived from inner          | flattened to `InvalidRequest` / `Internal` |

`Core` is split by the wrapped [`solti_core::CoreError`]: `InvalidSpec` → `INVALID_ARGUMENT` / `400` / `"InvalidRequest"`; everything else → `INTERNAL` / `500` / `"Internal"`.

HTTP error body:
```json
{ "error": "<label>", "message": "<detail>" }
```

HTTP requests return `413 Payload Too Large` with a JSON envelope (`{"error": "PayloadTooLarge", "message": "…"}`) when the body exceeds [`MAX_REQUEST_BYTES`](crate::MAX_REQUEST_BYTES) (4 MiB). 
gRPC calls return `RESOURCE_EXHAUSTED` for oversize messages. 
Script bodies are separately capped in the model at [`solti_model::MAX_SCRIPT_BODY_BYTES`] (2 MiB after base64 decode): oversize bodies are rejected as `InvalidRequest`.

## Feature flags

| Flag   | Enables                                                                           | Dependencies                            |
|--------|-----------------------------------------------------------------------------------|-----------------------------------------|
| `grpc` | `SoltiApiService`, `SoltiApiServer`, proto codegen                                | `tonic`, `tonic-prost`, `prost`         |
| `http` | `HttpApi`, axum router, proto-JSON serde                                          | `axum`, `serde_json`, `prost`, `pbjson` |
| `tls`  | `to_tonic_server_tls(&ServerTlsConfig)` adapter (under `grpc`); pulls `solti-tls` | `solti-tls`; activates `tonic/tls-ring` |

No feature is enabled by default. `tls` is additive on top of `grpc` (the adapter targets tonic; HTTP TLS is terminated by the binary via `axum-server`, see below).

### Enabling TLS

For gRPC:

```rust
use solti_api::{build_grpc_server, to_tonic_server_tls};
use solti_tls::ServerTlsConfig;

let server_tls = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")  // optional
    .build()?;

let tls_cfg = to_tonic_server_tls(&server_tls)?;
tonic::transport::Server::builder()
    .tls_config(tls_cfg)?
    .add_service(build_grpc_server(adapter))
    .serve("0.0.0.0:50443".parse()?)
    .await?;
```

For HTTP, terminate TLS in your binary via `axum-server` using the `rustls::ServerConfig` produced by `solti_tls::ServerTlsConfig::into_rustls_config()`.
See the `solti-tls` README for the full pattern.

## Build

`build.rs` walks `proto/` recursively, collecting every `*.proto` file (emitting `rerun-if-changed` for each). Two codegen passes:
- `tonic_prost_build::configure()`: message types always, tonic server/client only under `grpc`.
- `pbjson_build` under `http`: attaches canonical proto-JSON `Serialize`/`Deserialize` to the same message types, with `.emit_fields()` enabled so REST clients see `0` / `false` / `""` / `[]` / `{}` for default scalar/repeated/map values (optional `message` fields still omit on `None`).

The proto package selector lives at the top of `build.rs` as `const PROTO_PACKAGE = ".solti.v1";`. 
If the `package` declaration in a `.proto` changes, update this constant — otherwise pbjson generates nothing and HTTP compile fails. 
Adding new `.proto` files anywhere under `proto/` requires **no** changes to `build.rs`.

## Notes
- `ApiHandler` uses `async_trait` for object safety (`Send + Sync + 'static`).
- Both transports feed input through the same `convert_create_spec` validator.
- Re-exports: `solti_api::tonic`, `solti_api::axum` for version pinning.
- Proto contract in `proto/solti/v1/` (`api.proto`, `types.proto`).
