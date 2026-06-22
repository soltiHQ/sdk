# solti-tls

Shared TLS / mTLS configuration for Solti network-facing crates. 
Builders that hold *intent* (paths or in-memory PEM bytes) and produce `rustls::ServerConfig` / `rustls::ClientConfig` on demand. 

## Architecture

```text
 ┌──────────────────────┐         ┌──────────────────────┐
 │   PemSource::Path    │         │   PemSource::Bytes   │
 │ (read at into_*())   │         │ (already in memory)  │
 └─────────┬────────────┘         └──────────┬───────────┘
           │                                 │
           ▼                                 ▼
       ┌───────────────────────────────────────┐
       │     ServerTlsConfig / ClientTlsConfig │
       │     (Builder → built struct)          │
       └───────────────────┬───────────────────┘
                           │ into_rustls_config()
                           ▼
       ┌───────────────────────────────────────┐
       │  rustls::ServerConfig / ClientConfig  │
       │    (ALPN applied, mTLS verifier set)  │
       └───────────────────────────────────────┘
```

## Why this crate

- **Single source of truth**: one TLS config shape for every solti network-facing crate (`solti-api`, `solti-discover`, ...).
- **Lazy I/O**: paths are validated and read only at `into_rustls_config()`, `DiscoverConfig` can be cloned around freely before any cert is touched.
- **Both paths and bytes**: cert may live on disk or arrive in memory. 
- **Auto-installs `ring`**: calls `rustls::crypto::ring::default_provider().install_default()` if no `CryptoProvider` is set process-wide.
- **mTLS first-class**: server and client builders both have a single `require_client_ca…` / `client_cert…` knob.

## Quick start

### TLS-terminated server

```rust,ignore
use solti_tls::ServerTlsConfig;

let server_cfg = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .with_alpn(["h2"])                   // gRPC; ["h2", "http/1.1"] for HTTP
    .build()?
    .into_rustls_config()?;              // -> rustls::ServerConfig

// Plug into axum-server:
// axum_server::bind_rustls(addr, RustlsConfig::from_config(Arc::new(server_cfg)))
//     .serve(router.into_make_service()).await?;
```

### TLS-validating client

```rust,ignore
use solti_tls::ClientTlsConfig;

let client_cfg = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .with_alpn(["h2"])
    .build()?
    .into_rustls_config()?;              // -> rustls::ClientConfig

// Plug into reqwest:
// let http = reqwest::Client::builder().use_preconfigured_tls(client_cfg).build()?;
```

## mTLS

Server requires a client certificate signed by `clients-ca.crt`:

```rust,ignore
use solti_tls::ServerTlsConfig;

let server_cfg = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")
    .build()?
    .into_rustls_config()?;
```

Client presents its own certificate:

```rust,ignore
use solti_tls::ClientTlsConfig;

let client_cfg = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .client_cert_pem_file("/etc/solti/tls/agent.crt")
    .client_key_pem_file("/etc/solti/tls/agent.key")
    .build()?
    .into_rustls_config()?;
```

`client_cert` and `client_key` must be set together (or neither). Setting only
one returns `TlsError::MissingField("client_key")` / `MissingField("client_cert")`.

## Key types

| Type                      | Role                                                                |
|---------------------------|---------------------------------------------------------------------|
| `ServerTlsConfig`         | Server-side intent (cert, key, optional client-CA, ALPN)            |
| `ServerTlsConfigBuilder`  | Validated builder; missing required fields fail at `build()`        |
| `ClientTlsConfig`         | Client-side intent (CA, optional client cert+key, ALPN)             |
| `ClientTlsConfigBuilder`  | Validated builder; client cert/key must be paired                   |
| `PemSource`               | `Path(PathBuf)` or `Bytes(Vec<u8>)` — lazy read via `read()`        |
| `TlsError`                | `#[non_exhaustive]` error enum (see *Error model*)                  |
| `ensure_default_provider` | Idempotently installs `ring` as the rustls `CryptoProvider`         |
| `load_certs_from_pem`     | Pure parser: PEM bytes → `Vec<CertificateDer<'static>>`             |
| `load_key_from_pem`       | Pure parser: PEM bytes → `PrivateKeyDer<'static>`                   |

## PemSource: paths vs bytes

```rust,ignore
use solti_tls::PemSource;

// File on disk: read at into_rustls_config() time
let from_disk = PemSource::Path("/etc/solti/tls/server.crt".into());

// In-memory blob (e.g. from Vault, AWS Secrets Manager, env var)
let from_memory = PemSource::Bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec());
```

The builder convenience methods cover both forms:

| Path form                       | Bytes form                       |
|---------------------------------|----------------------------------|
| `cert_pem_file(path)`           | `cert_pem_bytes(bytes)`          |
| `key_pem_file(path)`            | `key_pem_bytes(bytes)`           |
| `ca_pem_file(path)`             | `ca_pem_bytes(bytes)`            |
| `require_client_ca_pem_file(p)` | `require_client_ca_pem_bytes(b)` |
| `client_cert_pem_file(path)`    | `client_cert_pem_bytes(bytes)`   |
| `client_key_pem_file(path)`     | `client_key_pem_bytes(bytes)`    |

## ALPN

ALPN protocols are stored in preference order and copied verbatim into `rustls::*Config::alpn_protocols`, as default is empty (no ALPN).

| Use case   | `with_alpn(...)`         |
|------------|--------------------------|
| gRPC only  | `["h2"]`                 |
| HTTP/2 + 1 | `["h2", "http/1.1"]`     |
| HTTP/1.1   | `["http/1.1"]`           |

## Integration patterns

### `solti-api` server (gRPC + tonic)

```rust,ignore
use solti_api::{build_grpc_server, to_tonic_server_tls};
use solti_tls::ServerTlsConfig;

let server_tls = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .build()?;

let tls_cfg = to_tonic_server_tls(&server_tls)?;
tonic::transport::Server::builder()
    .tls_config(tls_cfg)?
    .add_service(build_grpc_server(adapter))
    .serve("0.0.0.0:50443".parse()?)
    .await?;
```

Requires `solti-api` features `grpc + tls`.

### `solti-api` server (HTTP + axum-server)

axum does not terminate TLS itself; bind through `axum-server`:

```rust,ignore
use axum_server::tls_rustls::RustlsConfig;
use solti_api::HttpApi;
use solti_tls::ServerTlsConfig;

let rustls_cfg = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .with_alpn(["h2", "http/1.1"])
    .build()?
    .into_rustls_config()?;

let router = HttpApi::new(handler).router();
axum_server::bind_rustls(addr, RustlsConfig::from_config(Arc::new(rustls_cfg)))
    .serve(router.into_make_service()).await?;
```

`solti-tls` does **not** depend on `axum-server`; add it in your binary.

### `solti-discover` client (gRPC + HTTP)

`with_tls` accepts a `solti_tls::ClientTlsConfig` directly:

```rust,ignore
use solti_discover::DiscoverConfig;
use solti_tls::ClientTlsConfig;

let client_tls = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .client_cert_pem_file("/etc/solti/tls/agent.crt")
    .client_key_pem_file("/etc/solti/tls/agent.key")
    .build()?;

let cfg = DiscoverConfig::builder(/* ... */)
    .with_tls(client_tls)
    .build()?;
```

Internally, `solti-discover`:
- For HTTP (reqwest) uses `use_preconfigured_tls(rustls_config)`: passes the built `rustls::ClientConfig` straight in.
- For gRPC (tonic) converts to `tonic::transport::ClientTlsConfig`: tonic builds its own internal rustls config from PEM blobs.

Requires `solti-discover` features `tls + (http or grpc)`.

## Cryptography

- **Provider**: `ring` (selected via the `rustls` feature `ring`). 
- **Roots**: no `rustls-native-certs` integration. Trust roots come exclusively from PEM bundles you provide. 
  This is intentional: for service-to-service TLS in a controlled environment, the trust set is tightly defined.
- **Auto-install**: `into_rustls_config()` calls `ensure_default_provider()` which installs `ring` if no provider is set process-wide. 

## Security model

**Verified:** the server certificate chains to the CA you supply (`ClientTlsConfig`); under mTLS the client certificate chains to `require_client_ca(..)` and client auth is **mandatory** (`ServerTlsConfig`).

**Not verified here (caller's responsibility):**
- *Hostname / SAN* - matched by `rustls` at connect time against the `ServerName` you pass to `TlsConnector::connect(..)` / tonic / reqwest. 
  Pass the real server name; a wrong one silently weakens identity checking. 
  Do not install a `dangerous()` verifier.
- *Revocation* - no OCSP / CRL; rely on short cert lifetimes.
- *TLS versions / suites* - `rustls` safe defaults (TLS 1.2 + 1.3); no min-version policy. 
  Provider is `ring` (not configurable here).

**Secrets:** `PemSource::Bytes` may hold a private key; its `Debug` is redacted so
logging a config does not leak the key. Key bytes are not zeroized on drop.

## Error model

| Variant            | Cause                                                                     |
|--------------------|---------------------------------------------------------------------------|
| `Io`               | `PemSource::Path::read()` failed (`std::io::Error` is the source)         |
| `NoCertificates`   | PEM input parsed cleanly but contained no `CERTIFICATE` blocks            |
| `NoPrivateKey`     | PEM input parsed cleanly but contained no `PRIVATE KEY` block             |
| `MissingField`     | Builder rejected on `build()` (e.g. `cert`, `key`, paired `client_cert`)  |
| `Rustls`           | `rustls` rejected the configuration (cert/key mismatch, etc.)             |
| `ClientVerifier`   | `WebPkiClientVerifier::builder(...).build()` rejected the trust roots     |

All variants implement `std::error::Error`. The enum is `#[non_exhaustive]`.

## Feature flags

This crate has no public feature flags: `default = []`, all functionality is always built. 
Consumers of `solti-tls` (e.g. `solti-api`, `solti-discover`) gate their integration behind their own `tls` feature.

## Notes

- `into_rustls_config()` consumes `self`. Clone the config first if you need to keep the builder-side struct around (e.g. for diagnostics).
- `solti-tls` __does not configure session resumption, ticket keys, or OCSP stapling__: defaults from `rustls` apply.
