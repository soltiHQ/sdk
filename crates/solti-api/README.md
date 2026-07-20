# solti-api

Dual-transport API layer exposing task operations over gRPC and HTTP.

Both transports delegate to an `ApiHandler` trait, decoupling wire format from business logic. Both speak the same proto contract defined in `proto/solti/task/v1/`.

## Architecture
```text
 Control Plane / Client
     │
     ├──► gRPC (feature = "grpc")
     │        └──► TaskApiService<H>
     │                  │
     ├──► HTTP (feature = "http")
     │        └──► HttpApi<H> (axum Router)
     │                  │
     ▼                  ▼
 ApiHandler trait (transport-agnostic)
     └──► optional SupervisorApiAdapter (feature = "core-adapter")
                  │
                  ▼
         solti_core::SupervisorApi
```

## Versioning

`solti-api` exports `API_VERSION: u32` - the current protocol version.

Binary passes it to `solti_discover::DiscoverConfig::builder(... , API_VERSION)`, which reports it to the control-plane via `SyncRequest`. One binary = one API version.

```rust,ignore
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

| Type                   | Role                                                                             |
|------------------------|----------------------------------------------------------------------------------|
| `ApiHandler`           | Transport-agnostic trait with 7 operations (management, runs, and log stream)     |
| `OutputEventStream`    | `Pin<Box<dyn Stream<Item = OutputEvent> + Send>>` returned by `stream_task_logs` |
| `SupervisorApiAdapter` | Optional adapter bridging to `SupervisorApi` (feature `core-adapter`)            |
| `ApiError`             | Unified error mapped to gRPC Status / HTTP JSON                                  |
| `TaskApiService<H>`    | gRPC server impl (feature `grpc`)                                                |
| `HttpApi<H>`           | axum router builder (feature `http`)                                             |
| `BearerAuth`           | gRPC interceptor verifying the inbound bearer token (feature `grpc`)             |
| `API_VERSION`          | Protocol version constant reported via discover                                  |

## Error model

| Variant           | gRPC Status                    | HTTP Status                 | `error` label (HTTP body) |
|-------------------|--------------------------------|-----------------------------|---------------------------|
| `InvalidRequest`  | `INVALID_ARGUMENT`             | `400 Bad Request`           | `"InvalidRequest"`        |
| `Unauthenticated` | `UNAUTHENTICATED`              | `401 Unauthorized`          | `"Unauthenticated"`       |
| `AlreadyExists`   | `ALREADY_EXISTS`               | `409 Conflict`              | `"AlreadyExists"`         |
| `TaskNotFound`    | `NOT_FOUND`                    | `404 Not Found`             | `"TaskNotFound"`          |
| `PayloadTooLarge` | `RESOURCE_EXHAUSTED`           | `413 Payload Too Large`     | `"PayloadTooLarge"`       |
| `Internal`        | `INTERNAL`                     | `500 Internal Server Error` | `"Internal"`              |

With `core-adapter`, `CoreError` is translated at the adapter boundary:

- `InvalidSpec` → `InvalidRequest`;
- `AlreadyExists` → `AlreadyExists`;
- `NotFound` → `TaskNotFound`;
- everything else (`Supervisor`, `Mapping`, `Runner`, or a future variant) → `Internal`.

HTTP error body:
```json
{ "error": "<label>", "message": "<detail>" }
```

HTTP requests return `413 Payload Too Large` with a JSON envelope (`{"error": "PayloadTooLarge", "message": "…"}`) when the body exceeds [`MAX_REQUEST_BYTES`](crate::MAX_REQUEST_BYTES) (4 MiB). 
gRPC calls return `RESOURCE_EXHAUSTED` for oversize messages. 
Script bodies are separately capped in the model at [`solti_model::MAX_SCRIPT_BODY_BYTES`] (2 MiB after base64 decode): oversize bodies are rejected as `InvalidRequest`.

## Feature flags

| Flag           | Enables                                                         | Dependencies                            |
|----------------|-----------------------------------------------------------------|-----------------------------------------|
| `core-adapter` | `SupervisorApiAdapter`                                           | `solti-core`                            |
| `grpc`         | `GrpcApi`, `TaskApiService`, `TaskServiceServer`, proto codegen | `tonic`, `tonic-prost`, `prost`         |
| `grpc-tls`     | `to_tonic_server_tls(&ServerTlsConfig)`; implies `grpc`         | `solti-tls`; activates `tonic/tls-ring` |
| `http`         | `HttpApi`, axum router, proto-JSON serde                        | `axum`, `serde_json`, `prost`, `pbjson` |

No feature is enabled by default. `grpc-tls` implies `grpc`; HTTP TLS is
terminated by the binary via `axum-server`, not by this feature. The former
`tls` feature and `ApiError::Core` variant were removed in the stage 2 breaking
boundary cleanup.

### Enabling TLS

For gRPC:

```rust,no_run
use std::sync::Arc;

use solti_api::{GrpcApi, SupervisorApiAdapter, to_tonic_server_tls};
use solti_tls::ServerTlsConfig;

async fn serve_tls(adapter: Arc<SupervisorApiAdapter>) -> Result<(), Box<dyn std::error::Error>> {
    let server_tls = ServerTlsConfig::builder()
        .cert_pem_file("/etc/solti/tls/server.crt")
        .key_pem_file("/etc/solti/tls/server.key")
        .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt") // optional
        .build()?;

    let tls_cfg = to_tonic_server_tls(&server_tls)?;
    tonic::transport::Server::builder()
        .tls_config(tls_cfg)?
        .add_service(GrpcApi::new(adapter).server())
        .serve("0.0.0.0:50443".parse()?)
        .await?;
    Ok(())
}
```

For HTTP, terminate TLS in your binary via `axum-server` using the `rustls::ServerConfig` produced by `solti_tls::ServerTlsConfig::into_rustls_config()`.
See the `solti-tls` README for the full pattern.

### Enabling token auth

Require a bearer token on every inbound call. 
The token is the same shared secret the agent presents to the control plane in discovery (`solti_model::Token`). One config value gates both directions. 
Orthogonal to TLS; comparison is constant-time; a missing/invalid token is rejected with `401` (HTTP) / `Unauthenticated` (gRPC) before reaching any handler.

HTTP:

```rust,no_run
use std::sync::Arc;

use axum::Router;
use solti_api::{HttpApi, SupervisorApiAdapter};
use solti_model::Token;

fn secured_router(adapter: Arc<SupervisorApiAdapter>) -> Result<Router, Box<dyn std::error::Error>> {
    let router = HttpApi::new(adapter)
        .with_auth(Token::from_env("SOLTI_AGENT_TOKEN")?)
        .router();
    Ok(router)
}
```

gRPC:

```rust,no_run
use std::sync::Arc;

use solti_api::{GrpcApi, SupervisorApiAdapter};
use solti_model::Token;

async fn serve_grpc(adapter: Arc<SupervisorApiAdapter>) -> Result<(), Box<dyn std::error::Error>> {
    let svc = GrpcApi::new(adapter)
        .with_auth(Token::from_env("SOLTI_AGENT_TOKEN")?)
        .server();
    tonic::transport::Server::builder()
        .add_service(svc)
        .serve("0.0.0.0:50051".parse()?)
        .await?;
    Ok(())
}
```

When no token is configured (plain `HttpApi::new(...).router()` / `GrpcApi::new(...).server()`), no auth is enforced.

## Build

`build.rs` walks `proto/` recursively, collecting every `*.proto` file (emitting `rerun-if-changed` for each). Two codegen passes:
- `tonic_prost_build::configure()`: message types always, tonic server/client only under `grpc`.
- `pbjson_build` under `http`: attaches canonical proto-JSON `Serialize`/`Deserialize` to the same message types, with `.emit_fields()` enabled so REST clients see `0` / `false` / `""` / `[]` / `{}` for default scalar/repeated/map values (optional `message` fields still omit on `None`).

The proto package selector is derived in `build.rs` as `format!(".solti.task.v{API_MAJOR}")`. 
If the `package` declaration in a `.proto` changes, keep it in lockstep with `API_MAJOR` - otherwise pbjson generates nothing and HTTP compile fails. 
Adding new `.proto` files anywhere under `proto/` requires **no** changes to `build.rs`.

## Notes
- `ApiHandler` uses `async_trait` for object safety (`Send + Sync + 'static`).
- Both transports feed input through the same `convert_create_spec` validator.
- Re-exports: `solti_api::tonic`, `solti_api::axum` for version pinning.
- Proto contract in `proto/solti/task/v1/` (`api.proto`, `types.proto`).
