# solti-tls

> Shared TLS and mTLS config for Solti network crates.

`solti-tls` helps Solti crates use the same TLS shape everywhere. 
You can pass PEM files from disk or PEM bytes from memory. 
The crate builds a `rustls::ServerConfig` or `rustls::ClientConfig` only when you need it.

It is used by crates such as `solti-api` and `solti-discover`, but it has no dependency on them.

## The setup you stop repeating

TLS setup often becomes the same code in every binary:

```rust,ignore
let cert = std::fs::read("/etc/solti/tls/server.crt")?;
let key = std::fs::read("/etc/solti/tls/server.key")?;
// parse PEM, install provider, set ALPN, build rustls config...
```

With `solti-tls`, the binary only describes what it wants:

```rust,no_run
use solti_tls::ServerTlsConfig;

let server_tls = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .with_alpn(["h2"])
    .build()?;

let rustls_config = server_tls.into_rustls_config()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

No PEM is read during `build()`. 
File I/O and parsing happen in `into_rustls_config()`.

## Quick Start

### TLS Server

Use this for an API server that presents a certificate to clients:

```rust,no_run
use solti_tls::ServerTlsConfig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rustls_config = ServerTlsConfig::builder()
        .cert_pem_file("/etc/solti/tls/server.crt")
        .key_pem_file("/etc/solti/tls/server.key")
        .with_alpn(["h2"]) // gRPC. Use ["h2", "http/1.1"] for HTTP too.
        .build()?
        .into_rustls_config()?;

    // Pass `rustls_config` to tonic, axum-server, or tokio-rustls.
    let _ = rustls_config;
    Ok(())
}
```

### TLS Client

Use this for a client that verifies the server certificate with your CA bundle:

```rust,no_run
use solti_tls::ClientTlsConfig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rustls_config = ClientTlsConfig::builder()
        .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
        .with_alpn(["h2"])
        .build()?
        .into_rustls_config()?;

    // Pass `rustls_config` to reqwest, tonic, or tokio-rustls.
    let _ = rustls_config;
    Ok(())
}
```

## Add mTLS

mTLS means both sides have certificates.

On the server, require client certificates signed by a client CA:

```rust,no_run
use solti_tls::ServerTlsConfig;

let server_tls = ServerTlsConfig::builder()
    .cert_pem_file("/etc/solti/tls/server.crt")
    .key_pem_file("/etc/solti/tls/server.key")
    .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")
    .build()?;
# let _ = server_tls;
# Ok::<(), Box<dyn std::error::Error>>(())
```

On the client, send a client certificate and key:

```rust,no_run
use solti_tls::ClientTlsConfig;

let client_tls = ClientTlsConfig::builder()
    .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    .client_cert_pem_file("/etc/solti/tls/agent.crt")
    .client_key_pem_file("/etc/solti/tls/agent.key")
    .build()?;
# let _ = client_tls;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`client_cert` and `client_key` must be set together. If only one is set, `build()` returns `TlsError::MissingField`.

## Why solti-tls?

- **One shape**: server and client TLS config look the same across Solti crates.
- **Lazy I/O**: paths are read only when `into_rustls_config()` runs.
- **Paths or bytes**: load certs from disk, Vault, env vars, or any secret store.
- **mTLS is first-class**: one builder call enables client certificate checks.
- **rustls defaults**: config is built on `rustls` safe defaults.
- **Provider install**: `ring` is installed as the process-wide rustls provider if none is set.

## When to Use It

Use this crate when your binary needs TLS config for:
- a Solti API server;
- an agent that connects to a control plane;
- a test or demo that wants in-memory PEM bytes;
- any service that wants one small TLS config type instead of custom setup code.

This crate does not open sockets. It only builds `rustls` configs.

## How It Works

```text
PemSource::Path                 PemSource::Bytes
       \                        /
        \                      /
         v                    v
    ServerTlsConfig / ClientTlsConfig
         |
         | into_rustls_config()
         v
    rustls::ServerConfig / rustls::ClientConfig
```

`build()` validates required fields. `into_rustls_config()` reads PEM, parses certs and keys, applies ALPN, and builds the final `rustls` config.

## Main Types

| Area      | Types                                       |
|-----------|---------------------------------------------|
| Server    | `ServerTlsConfig`, `ServerTlsConfigBuilder` |
| Client    | `ClientTlsConfig`, `ClientTlsConfigBuilder` |
| PEM input | `PemSource`                                 |
| Parsing   | `load_certs_from_pem`, `load_key_from_pem`  |
| Provider  | `ensure_default_provider`                   |
| Errors    | `TlsError`                                  |

## PEM Sources

A PEM source can be a file path or bytes:

```rust
use solti_tls::PemSource;

let from_disk = PemSource::Path("/etc/solti/tls/server.crt".into());
let from_memory = PemSource::Bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec());

assert!(format!("{from_memory:?}").contains("redacted"));
# let _ = from_disk;
```

Builder methods exist for both forms:

| File method                        | Bytes method                         |
|------------------------------------|--------------------------------------|
| `cert_pem_file(path)`              | `cert_pem_bytes(bytes)`              |
| `key_pem_file(path)`               | `key_pem_bytes(bytes)`               |
| `ca_pem_file(path)`                | `ca_pem_bytes(bytes)`                |
| `require_client_ca_pem_file(path)` | `require_client_ca_pem_bytes(bytes)` |
| `client_cert_pem_file(path)`       | `client_cert_pem_bytes(bytes)`       |
| `client_key_pem_file(path)`        | `client_key_pem_bytes(bytes)`        |

`PemSource::Bytes` may hold private keys. Its `Debug` output is redacted, but the bytes are not zeroized on drop.

## ALPN

ALPN protocols are copied into `rustls` in the order you pass them:

| Use case            | Value                |
|---------------------|----------------------|
| gRPC only           | `["h2"]`             |
| HTTP/2 and HTTP/1.1 | `["h2", "http/1.1"]` |
| HTTP/1.1 only       | `["http/1.1"]`       |

The default is empty: no ALPN is requested.

## Security Model

What this crate verifies:
- A client config verifies that the server certificate chains to the CA bundle you pass.
- A server config with `require_client_ca_*` requires a client certificate signed by that CA.
- Trust roots come only from your PEM. The OS trust store is not used.

What the caller must still do:
- Pass the real server name when connecting. Hostname and SAN checks happen at connect time, not when this config is built.
- Do not replace the verifier with a `dangerous()` one unless you are writing a controlled test.
- Handle certificate rotation and revocation policy. This crate does not check OCSP or CRLs.
- Choose short certificate lifetimes if you need a simple revocation story.

TLS protocol versions and cipher suites come from `rustls` safe defaults. The provider is `ring`.

## Error Handling

`TlsError` covers all fallible work in this crate:

| Variant          | Meaning                                                               |
|------------------|-----------------------------------------------------------------------|
| `Io`             | Could not read a PEM file, or PEM parsing returned an I/O-style error |
| `NoCertificates` | A cert or CA PEM had no `CERTIFICATE` blocks                          |
| `NoPrivateKey`   | A key PEM had no private key block                                    |
| `MissingField`   | A required builder field was missing                                  |
| `Rustls`         | `rustls` rejected the final config                                    |
| `ClientVerifier` | The mTLS client verifier could not be built                           |

The enum is `#[non_exhaustive]`, so match it with a wildcard arm.

## Integration Notes

For `solti-api` gRPC, use `solti_api::to_tonic_server_tls` with a `ServerTlsConfig`.

For `solti-api` HTTP, build a `rustls::ServerConfig` and pass it to `axum-server`.
`solti-tls` does not depend on `axum-server`; add that in your binary.

For `solti-discover`, pass `ClientTlsConfig` to `DiscoverConfigBuilder::with_tls(...)`.
The discover crate converts it for HTTP or gRPC based on enabled features.
