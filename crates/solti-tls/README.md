# solti-tls

`solti-tls` defines TLS and mutual TLS material for Solti transports: `solti-api` builds a server config, `solti-discover` builds a client config.
It validates certificates, private keys, and trust roots, then produces a `rustls` configuration or loaded PEM for a transport adapter.

## Quick start

```rust,no_run
use solti_tls::{ServerTlsConfig, TlsIdentity};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ))
    .into_rustls_config()?;

    config.alpn_protocols = vec![b"h2".to_vec()]; // the transport sets ALPN
    Ok(())
}
```

Most callers use `into_rustls_config()`. 
Use `load()` instead when an adapter (`reqwest`, `tonic`) consumes raw PEM rather than `rustls` types.

## What it does

- keeps a certificate chain and private key together as one identity;
- gives HTTP, gRPC, and other adapters the same TLS input;
- validates material before transport starts;
- separates identities from trust roots;
- makes client authentication explicit;
- accepts PEM from files or memory.

## Inputs and outputs

| Value             | Input                                           |
|-------------------|-------------------------------------------------|
| `TlsIdentity`     | Certificate chain and private key               |
| `TrustRoots`      | Certificates trusted for peer verification      |
| `ServerTlsConfig` | Server identity and optional client trust roots |
| `ClientTlsConfig` | Server trust roots and optional client identity |

| Method                 | Output                                                              |
|------------------------|---------------------------------------------------------------------|
| `into_rustls_config()` | `rustls::ServerConfig` or `rustls::ClientConfig`                    |
| `load()`               | Validated PEM in `LoadedServerTlsConfig` or `LoadedClientTlsConfig` |

Constructors only store their inputs. Files are read by `load()` or `into_rustls_config()`.

## Server

A server always requires an identity. 
Client trust roots enable mandatory client authentication.

```rust,no_run
use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ));

    // TLS without client authentication.
    let plain = server.clone().into_rustls_config()?;

    // Mutual TLS. Every client must present a trusted certificate.
    let mtls = server
        .require_client_auth(TrustRoots::from_pem_file(
            "/etc/solti/tls/clients-ca.crt",
        ))
        .into_rustls_config()?;

    assert!(plain.alpn_protocols.is_empty());
    assert!(mtls.alpn_protocols.is_empty());
    Ok(())
}
```

## Client

A client always requires roots for server verification. 
A client identity enables mutual TLS.

```rust,no_run
use solti_tls::{ClientTlsConfig, TlsIdentity, TrustRoots};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ClientTlsConfig::new(TrustRoots::from_pem_file(
        "/etc/solti/tls/control-plane-ca.crt",
    ));

    // TLS without a client certificate.
    let plain = client.clone().into_rustls_config()?;

    // Mutual TLS. The client presents this identity to the server.
    let mtls = client
        .with_identity(TlsIdentity::from_pem_files(
            "/etc/solti/tls/agent.crt",
            "/etc/solti/tls/agent.key",
        ))
        .into_rustls_config()?;

    assert!(plain.alpn_protocols.is_empty());
    assert!(mtls.alpn_protocols.is_empty());
    Ok(())
}
```

The client trusts only the configured roots. 
Operating-system roots are not added automatically.

## PEM sources

The convenience constructors accept either paths or bytes:

```rust
use solti_tls::{TlsIdentity, TrustRoots};

let identity = TlsIdentity::from_pem_bytes(
    b"certificate PEM".to_vec(),
    b"private-key PEM".to_vec(),
);
let roots = TrustRoots::from_pem_bytes(b"CA certificate PEM".to_vec());

let _ = (identity, roots);
```

Use `TlsIdentity::new`, `PemSource`, and `PrivateKeySource` when certificate and private-key sources must be assembled separately.

## Transport adapters

`load()` returns validated PEM when an adapter does not consume `rustls` configuration types directly:

```rust,no_run
use solti_tls::{ServerTlsConfig, TlsIdentity};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let loaded = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ))
    .load()?;

    let certificate_chain = loaded.identity().certificate_chain_pem();
    let private_key = loaded.identity().expose_private_key_pem();
    let _ = (certificate_chain, private_key);
    Ok(())
}
```

## Specific behavior

- In-memory private keys and loaded private-key PEM are redacted in `Debug` and zeroized when their final crate-owned buffer is dropped.
- Generated `rustls` configurations have no ALPN protocols. The transport sets `h2`, `http/1.1`, or another protocol.
- `load()` and `into_rustls_config()` parse PEM and validate identities and trust roots.
- `require_client_auth()` makes a trusted client certificate mandatory.
- Server-name and SAN validation happens during the client handshake.
- A transport or TLS library may retain its own private-key copy.
- OCSP and CRL validation are not configured by this crate.
- The first configuration build installs the `ring` `rustls` provider as the process default; an existing default is left untouched.
- TLS 1.3 and TLS 1.2 are both accepted; this crate does not expose a version knob.

## Errors

`TlsError` separates file, PEM, certificate, and configuration failures:

- `InvalidPem`, `NoCertificates`, `NoPrivateKey`, and `MultiplePrivateKeys` describe malformed material;
- `InvalidCertificate` identifies a rejected trust root;
- `Configuration` identifies an invalid certificate/private-key pair;
- `ClientVerifier` identifies invalid client-authentication roots;
- `ReadPem` includes the PEM role and path.
