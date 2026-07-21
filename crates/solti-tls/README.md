# solti-tls

Shared declarative TLS and mTLS configuration for Solti transports.

`solti-tls` owns certificate sources, private-key sources, identities, trust roots, loading, PEM validation, and conversion to rustls. 

## Server

```rust,no_run
use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let identity = TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    );

    let server = ServerTlsConfig::new(identity)
        .require_client_auth(TrustRoots::from_pem_file(
            "/etc/solti/tls/clients-ca.crt",
        ));

    let mut rustls = server.into_rustls_config()?;
    // The HTTP transport owns its application-protocol policy.
    rustls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(())
}
```

Omit `require_client_auth` for ordinary server TLS. When it is present, a valid client certificate is mandatory.

## Client

```rust,no_run
use solti_tls::{ClientTlsConfig, TlsIdentity, TrustRoots};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ClientTlsConfig::new(TrustRoots::from_pem_file(
        "/etc/solti/tls/control-plane-ca.crt",
    ))
    .with_identity(TlsIdentity::from_pem_files(
        "/etc/solti/tls/agent.crt",
        "/etc/solti/tls/agent.key",
    ));

    let rustls = client.into_rustls_config()?;
    let _ = rustls;
    Ok(())
}
```

Omit `with_identity` for ordinary client TLS. The client trusts only the configured roots; it does not implicitly use the operating-system trust store.

## Model

```text
PemSource + PrivateKeySource
            |
            v
       TlsIdentity              PemSource
            |                       |
            +--------+--------------+
                     v
       ServerTlsConfig / ClientTlsConfig
                  |             |
                load()   into_rustls_config()
                  |             |
                  v             v
          loaded PEM       rustls config
          for adapters
```

- `load()` is the transport-adapter boundary for tonic and similar libraries.
- `TlsIdentity` is always a complete certificate-chain/private-key pair.
- `TrustRoots` distinguishes trust anchors from an identity certificate.
- `ServerTlsConfig` always has a server identity.
- `ClientTlsConfig` always has server trust roots.
- PEM parser and process-wide rustls-provider setup are internal details.

## Sources and private keys

```rust
use solti_tls::{PemSource, PrivateKeySource, TlsIdentity};

let certificate = PemSource::bytes(b"certificate PEM".to_vec());
let private_key = PrivateKeySource::bytes(b"private-key PEM".to_vec());
let identity = TlsIdentity::new(certificate, private_key);

assert!(format!("{identity:?}").contains("redacted"));
```

Certificate and trust-root bytes are shared across config clones. Private-key bytes have a separate source type and are zeroized when the final source or loaded-material owner is dropped. A downstream TLS library may make its own copy after the bytes cross the adapter boundary.

## ALPN

ALPN is not part of `ClientTlsConfig` or `ServerTlsConfig`. It describes an application transport, not certificate or trust policy:

- tonic owns gRPC's `h2` configuration;
- HTTP adapters set `h2` and/or `http/1.1` on their rustls config;
- a direct rustls caller sets `alpn_protocols` after conversion.

## Errors

`TlsError` distinguishes:

- `ReadPem`: filesystem failure with the PEM role and exact path;
- `InvalidPem`: malformed PEM syntax;
- `NoCertificates`, `NoPrivateKey`, `MultiplePrivateKeys`: invalid material shape;
- `InvalidCertificate`: a rejected trust anchor;
- `Configuration`: a rejected certificate/private-key identity;
- `ClientVerifier`: failure to build mandatory mTLS client verification.

The enum is `#[non_exhaustive]`.

Hostname and SAN validation happens when connecting, against the server name
provided to rustls, tonic, or reqwest. OCSP and CRL checking are not configured
by this crate.
