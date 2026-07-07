# solti-discover

Periodic heartbeat that registers an agent with the control plane and reports liveness and platform telemetry.
Dual-transport (gRPC + HTTP).

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

`DiscoverConfig` accepts `api_version: u32` from the binary (passed into `SyncRequest.api_version`).
The proto field is `int32`: the control-plane interprets `1 = v1`.

```rust,ignore
use solti_api::API_VERSION;

let cfg = DiscoverConfig::builder(
    agent_id, name, agent_endpoint, control_plane_endpoint,
    DiscoveryTransport::Grpc, 60_000, API_VERSION,
).build()?;
```

The binary is the integration point: solti-discover does not depend on solti-api.

## Key types

| Type                    | Role                                                                  |
|-------------------------|-----------------------------------------------------------------------|
| `DiscoverConfig`        | Agent identity, endpoint, transport, interval, capabilities           |
| `DiscoverConfigBuilder` | Validated builder; enforces invariants on `build()`                   |
| `DiscoveryTransport`    | Selects gRPC or HTTP path                                             |
| `Token`                 | Bearer secret presented to the CP (re-export of `solti_model::Token`) |
| `DiscoverError`         | Config, transport, parse, and rejection failures                      |
| `sync()`                | Factory returns `Result<(TaskRef, TaskSpec), DiscoverError>`          |
| `SyncRequest`           | Protobuf message sent each cycle                                      |
| `SyncResponse`          | Protobuf ack: `success`, optional `reason`, `retry_after_s`           |

## Sync protocol

Per-version protocol details: [sync_v1.md](sync_v1.md).

## Error model

| Variant           | Feature | Cause                                                                                      |
|-------------------|---------|--------------------------------------------------------------------------------------------|
| `InvalidConfig`   | -       | Builder-stage validation failure                                                           |
| `SpecBuild`       | -       | `TaskSpec::builder(...).build()` rejected the spec                                         |
| `GrpcTransport`   | `grpc`  | TCP / TLS / HTTP2 connection failure                                                       |
| `GrpcStatus`      | `grpc`  | Server returned non-OK gRPC status                                                         |
| `HttpRequest`     | `http`  | HTTP-level failure (connection, timeout, reqwest builder)                                  |
| `HttpStatus`      | `http`  | Non-2xx HTTP status (body truncated to 1 KiB)                                              |
| `InvalidResponse` | `http`  | Response body failed JSON deserialization                                                  |
| `Rejected`        | -       | Control plane returned `success: false`, with `reason`/`retry_after_s`                     |
| `AuthFailed`      | -       | CP rejected the credential (HTTP 401/403, gRPC Unauthenticated/PermissionDenied); terminal |

## Feature flags

| Flag   | Enables                                                        | Dependencies                                                             |
|--------|----------------------------------------------------------------|--------------------------------------------------------------------------|
| `grpc` | gRPC transport (tonic client)                                  | `tonic`, `tonic-prost`, `prost`                                          |
| `http` | HTTP transport (reqwest + canonical proto-JSON)                | `reqwest`, `serde_json`, `prost`, `pbjson`                               |
| `tls`  | Adds `with_tls(...)` builder method (TLS / mTLS for transport) | `solti-tls`; activates `tonic/tls-ring` and `reqwest/rustls-no-provider` |

No feature is enabled by default. `tls` is additive on top of `grpc`/`http`.

### Enabling TLS

```rust,no_run
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_model::AgentId;
use solti_tls::ClientTlsConfig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client_tls = ClientTlsConfig::builder()
        .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
        .client_cert_pem_file("/etc/solti/tls/agent.crt") // optional, for mTLS
        .client_key_pem_file("/etc/solti/tls/agent.key")
        .build()?;

    let _cfg = DiscoverConfig::builder(
        AgentId::from("agent-1"),
        "agent-1",
        "https://127.0.0.1:8085",
        "https://control-plane:9000",
        DiscoveryTransport::Http,
        30_000, // sync interval (ms)
        1,      // api_version
    )
    .with_tls(client_tls)
    .build()?;
    Ok(())
}
```

For HTTP (reqwest), the built `rustls::ClientConfig` is plugged in via `use_preconfigured_tls`. 
For gRPC (tonic), PEM bytes are re-shaped into `tonic::transport::ClientTlsConfig` (tonic builds its own internal rustls config). 
See the `solti-tls` README for the full integration story.

### Enabling token auth

```rust,no_run
use solti_discover::{DiscoverConfig, DiscoveryTransport, Token};
use solti_model::AgentId;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _cfg = DiscoverConfig::builder(
        AgentId::from("agent-1"),
        "agent-1",
        "https://127.0.0.1:8085",
        "https://control-plane:9000",
        DiscoveryTransport::Http,
        30_000, // sync interval (ms)
        1,      // api_version
    )
    .with_token(Token::from_env("SOLTI_AGENT_TOKEN")?)
    .build()?;
    Ok(())
}
```

The token is sent as `Authorization: Bearer <token>` (HTTP) / `authorization` metadata (gRPC) on every sync - never in the request body.
It stays out of the wire schema. Orthogonal to TLS: all four modes (±token, ±tls) work, but the sync task logs a warning if a token is set without TLS (the credential would travel in plaintext). 
The agent's inbound API (`solti-api`) verifies the same secret: one config value enables auth in both directions.

## Task policy

The sync task is created with:
- `RestartPolicy::periodic(delay_ms)` - runs on interval
- `BackoffPolicy` (default: equal jitter, `first_ms = delay_ms/2`, `max_ms = delay_ms*3`, factor 2.0) - overridable via `DiscoverConfigBuilder::backoff`
- `AdmissionPolicy::Replace` new sync replaces a stale one
- Slot: `solti-discover-sync`

## Server-advised backoff (`retry_after_s`)

When the control plane responds with `success = false` and a non-zero `retry_after_s`, the agent stores a Unix deadline in its in-memory sync context. 
Before sending the next request, the task waits until that deadline has passed.

The wait is *sequential*, not a max: the supervisor's `BackoffPolicy` first spaces the task *restart*, and then the next attempt body waits out any *remaining* server hold before sending. 
Effective gap is:

```text
next_attempt_wait ≈ client_backoff + remaining(server_retry_after_s)
```

- `retry_after_s = 0` (unspecified) - client falls back to its configured backoff only.
- The deadline is cleared on the next successful sync.
- The deadline is in-memory; an agent restart drops it.

## Timeouts

Both transports honor the timeouts from `DiscoverConfig`:

| Field                 | Default        | Applies to                                                             |
|-----------------------|----------------|------------------------------------------------------------------------|
| `connect_timeout_ms`  | `5_000`        | TCP/TLS handshake (reqwest `connect_timeout`, tonic `connect_timeout`) |
| `request_timeout_ms`  | `30_000`       | End-to-end request (reqwest `timeout`, tonic `timeout`)                |

Override via `DiscoverConfigBuilder::connect_timeout_ms` / `request_timeout_ms`.

## Build

`build.rs` walks `proto/` recursively, collecting every `*.proto` file (plus
emitting `rerun-if-changed` for each). Two codegen passes:

- `tonic_prost_build::configure()` - message types always, tonic server/client only under `grpc`.
- `pbjson_build` under `http` - attaches canonical proto-JSON `Serialize`/`Deserialize` to the same message types.

The proto package selector lives at the top of `build.rs` as `const PROTO_PACKAGE = ".solti.discover.v1";`. 
If the `package` declaration in a `.proto` changes, update this constant. Adding new `.proto` files anywhere under `proto/` requires **no** changes to `build.rs`.

## Notes

- gRPC channel is lazily created via `OnceCell` and reused across cycles (connection pooling).
- HTTP `reqwest::Client` is built once with `connect_timeout` + `timeout` + `User-Agent` (`solti-discover/<version>`) and reused for the same effect.
- HTTP sync path is derived from `api_version`: `/api/v{n}/discovery/sync`. Changing `api_version` automatically changes the endpoint.
- Cancellation is cooperative via `tokio::select!` on the cancel token and the network future (and, when honoring a server-advised hold, on the sleep).
- `os_info()` reads `/etc/os-release`, falls back to `/usr/lib/os-release` (freedesktop spec), then to `std::env::consts::OS`. Linux only; other platforms return the platform string.
- `SyncContext` is wrapped in `Arc` and shared into the async task closure. It carries the base request, both clients, and the `retry_hold_until: AtomicU64` deadline honored on the next attempt.
- `tonic-prost` is a regular `[dependencies]` entry (feature-gated) - generated gRPC code references `tonic_prost::ProstCodec` at runtime.
