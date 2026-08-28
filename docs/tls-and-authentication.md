---
title: TLS, authentication, and authorization
description: Configure transport trust, request identities, and Task API permissions at their separate ownership boundaries.
---

# TLS, authentication, and authorization

TLS protects a connection. Authentication identifies a request's caller. Authorization decides whether that caller may perform an operation.
The SDK exposes separate components for these jobs; enabling one does not configure the others.

## Choose the boundary

| Concern | SDK participant | Application responsibility |
|---------|-----------------|----------------------------|
| TLS material | `solti-tls`: identities and trust roots | Provide certificates and keys; choose trusted peers and handle material updates. |
| TLS transport | The HTTP host, tonic adapter, or discovery client | Own connections, server names, listener settings, and the application protocol. |
| Request authentication | `solti-api::ApiAuthenticator` or static bearer token | Validate credentials and assign application identity attributes. |
| Operation authorization | `solti-api::ApiAuthorizer` | Decide which operations and targets an identity may access. |
| Outbound discovery credentials | `DiscoverConfigBuilder` | Choose the control-plane token, trust roots, and optional client identity. |

`solti_tls::TlsIdentity` contains a certificate chain and key.
`solti_api::ApiIdentity` contains an optional subject and application-defined attributes.
They are different types with different purposes.
Any mapping from a trusted peer certificate to application permissions belongs to the application.

## Select features

| Need | Direct crate feature | `solti` facade feature |
|------|----------------------|------------------------|
| TLS configuration values | `solti-tls` has no optional features | `tls` |
| HTTP access-control hooks | `solti-api/http` | `api-http` |
| gRPC access-control hooks | `solti-api/grpc` | `api-grpc` |
| Tonic server TLS adapter | `solti-api/grpc-tls` | `api-grpc-tls` |
| Discovery HTTP custom TLS | `solti-discover/http,tls` | `discover-http-tls` |
| Discovery gRPC HTTPS | `solti-discover/grpc,tls` | `discover-grpc-tls` |

HTTP TLS is not a `HttpApi` builder option.
The application hosts its router behind a TLS-capable server or another application-owned TLS boundary.
See [serving the API](serving-api.md) for the router and listener roles.

## Load a server identity

A server always needs a certificate chain and matching private key:

```rust
use solti_tls::{ServerTlsConfig, TlsIdentity};

fn server_material() -> ServerTlsConfig {
    ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ))
}
```

Constructors only store the sources. They do not read the files.
Use `load()` to receive validated PEM for an adapter, or `into_rustls_config()` to receive a Rustls configuration.
Both read and validate the sources at that call.
Supply the end-entity certificate first, followed by its issuer chain.

File, PEM, missing-certificate/key, trust-root, and identity failures are reported through `TlsError`.
A successful configuration build does not validate a remote server name; the client checks that name during its handshake.

## Make client certificates mandatory

Server-side mTLS requires roots for client verification.
The client separately requires roots for the server and an identity to present:

```rust
use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};

fn mutual_tls_material() -> (ServerTlsConfig, ClientTlsConfig) {
    let server = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ))
    .require_client_auth(TrustRoots::from_pem_file(
        "/etc/solti/tls/client-ca.crt",
    ));

    let client = ClientTlsConfig::new(TrustRoots::from_pem_file(
        "/etc/solti/tls/server-ca.crt",
    ))
    .with_identity(TlsIdentity::from_pem_files(
        "/etc/solti/tls/client.crt",
        "/etc/solti/tls/client.key",
    ));

    (server, client)
}
```

Without `require_client_auth`, the server does not request a client certificate.
With it, every client must present a certificate accepted by those roots.
Adding an identity to the client does not make an independently configured server require it.

`ClientTlsConfig` trusts only its configured roots. It does not add operating-system roots.
This differs from [discovery's default HTTPS client](discovery.md#select-features), which uses platform roots when custom TLS is absent.

## Adapt material to a transport

Generated Rustls configurations have an empty `alpn_protocols` list.
The transport selects protocols such as `h2` or `http/1.1` and supplies the expected server name when connecting.
The [TLS round-trip example](../crates/solti-tls/examples/tls_round_trip.rs) shows both steps and joins both peers after their exchange.

For tonic, enable `solti-api/grpc-tls` and convert the shared server settings:

```rust
use solti_api::to_tonic_server_tls;
use solti_tls::{ServerTlsConfig, TlsError};

fn tonic_tls(
    material: ServerTlsConfig,
) -> Result<solti_api::tonic::transport::ServerTlsConfig, TlsError> {
    to_tonic_server_tls(material)
}
```

The application passes this result to its tonic server builder.
The adapter loads and validates the material first. Configured client roots remain mandatory client authentication.
It does not start the gRPC server.

For outbound discovery, pass `ClientTlsConfig` through `DiscoverConfigBuilder::with_tls`.
Custom TLS requires an HTTPS control-plane endpoint.
The HTTP discovery adapter sets its own ALPN and reuses the client across attempts.

## Authenticate Task API requests

Task API authentication is disabled until the application calls `with_auth` or `with_authenticator`.
The static-token path is useful when the application deliberately uses one shared credential:

```rust
use std::sync::Arc;
use solti_api::{ApiHandler, HttpApi};
use solti_model::Token;

fn authenticated_http<H: ApiHandler>(
    handler: Arc<H>,
    token: Token,
) -> solti_api::axum::Router {
    HttpApi::new(handler).with_auth(token).router()
}
```

The binary supplies the token; this function does not choose or load a secret.
HTTP expects `Authorization: Bearer <token>`.
The documented gRPC contract uses the same value in `authorization` metadata.
The scheme comparison is case-insensitive.

A rejected credential does not reach `ApiHandler`.
HTTP returns `401` with `WWW-Authenticate: Bearer`; gRPC returns `Unauthenticated`.
The static token produces an authenticated identity without an individual subject.
It proves knowledge of a shared secret, not which user sent the request.

For application identities, implement `ApiAuthenticator::authenticate` and install it with `with_authenticator`.
The hook receives the transport and optional bearer credential.
It returns `ApiIdentity::for_subject(...)`, optional attributes, or an `ApiError`.
Return `ApiError::Unauthenticated` for missing or invalid credentials.
The credential is borrowed from the request and should not be retained or logged.

`with_auth` and `with_authenticator` replace the same configured authentication hook; they do not form two authentication stages.
In HTTP, the configured authenticator runs before body extraction and inserts its returned identity into request extensions.
When no Task API authenticator is configured, application middleware can supply an `ApiIdentity` through those extensions.

## Authorize the operation

Install `ApiAuthorizer` independently with `with_authorizer`.
It runs after validation and before the handler operation.

| Input | Meaning |
|-------|---------|
| Identity | Optional authenticated subject and application attributes. It can be absent when no authentication layer supplied it. |
| `TaskOperation` | Create, apply, get, list, watch, list runs, cancel, delete, or stream logs. |
| `TaskTarget` | The validated desired manifest, one task name, or the complete collection. |

This example policy allows only authenticated read operations:

```rust
use async_trait::async_trait;
use solti_api::{ApiAuthorizer, ApiError, AuthorizationRequest, TaskOperation};

struct AuthenticatedReadOnly;

#[async_trait]
impl ApiAuthorizer for AuthenticatedReadOnly {
    async fn authorize(&self, request: AuthorizationRequest<'_>) -> Result<(), ApiError> {
        if request.identity().is_none() {
            return Err(ApiError::Unauthenticated("identity required".into()));
        }

        if matches!(
            request.operation(),
            TaskOperation::Get
                | TaskOperation::List
                | TaskOperation::Watch
                | TaskOperation::ListRuns
                | TaskOperation::StreamLogs
        ) {
            Ok(())
        } else {
            Err(ApiError::Forbidden("read-only policy".into()))
        }
    }
}
```

Install it with `.with_authorizer(Arc::new(AuthenticatedReadOnly))` on the transport builder.
The snippet needs the `async-trait` dependency in addition to `solti-api`.
This is an example application policy, not a built-in SDK role.
Normal policy denial uses `Forbidden`: HTTP `403`, gRPC `PermissionDenied`.
Other errors can report a failed or unavailable policy backend.

List and Watch authorization covers the collection operation.
The hook does not filter individual items or events, and it does not receive a tenant-scoped view automatically.
An application that needs row-level visibility must implement that visibility at its backend boundary.
Streams are authorized when opened, not again for each event.
Changing external permissions does not cause the hook to re-check an already-open stream.

Solti does not define user storage, roles, tenants, RBAC rules, or policy persistence.
API authentication and authorization also do not configure subprocess or container isolation.
See [containers and isolation](containers-and-isolation.md).

## Keep inbound and outbound policy separate

Configuring Task API bearer authentication does not enable TLS on its listener.
Configure both through their respective owners when the application needs them.

Discovery has its own credential-transport check: a token requires an HTTPS control-plane endpoint by default.
`allow_insecure_token_transport()` explicitly permits plaintext for controlled development or loopback and emits a warning.
Plaintext discovery without a token remains supported.
An inbound Task API token and an outbound discovery token are separate settings; neither installs the other.

The [metrics endpoint](observability.md#serve-the-registry) is another independent boundary.
It is plaintext and unauthenticated. Task API hooks do not protect it.

## Own material lifetime

- File sources are loaded when a configuration is built. Replacing a PEM file does not update an already-built client or server configuration.
- For discovery, rebuild the task with new TLS settings and a new revision when captured material changes.
- In-memory private-key sources and loaded PEM redact private-key bytes in `Debug` and zeroize their final crate-owned buffers on drop.
- A transport or TLS library can retain its own private-key copy. Zeroization here does not erase those copies.
- The first configuration build installs the Ring Rustls provider only if no process default exists. An existing default is preserved.
- The crate configures neither OCSP nor CRL validation. Its current dependency configuration enables TLS 1.2 and TLS 1.3; it has no public protocol-version setting.

The application owns certificate issuance, file access, replacement policy, server restart or configuration swap, and connection shutdown.

Source: [TLS client](../crates/solti-tls/src/client.rs), [TLS server](../crates/solti-tls/src/server.rs), [identity material](../crates/solti-tls/src/identity.rs), [access-control traits](../crates/solti-api/src/auth.rs), [HTTP hooks](../crates/solti-api/src/http.rs), and [tonic TLS adapter](../crates/solti-api/src/tls.rs).

## Run the material and handshake examples

These examples generate development certificates or use temporary local files. They do not require a deployed PKI:

```sh
cargo run -p solti-tls --example pem_sources
cargo run -p solti-tls --example tls_round_trip
cargo run -p solti-tls --example mtls_round_trip
```

See [PEM sources](../crates/solti-tls/examples/pem_sources.rs), [TLS](../crates/solti-tls/examples/tls_round_trip.rs), and [mutual TLS](../crates/solti-tls/examples/mtls_round_trip.rs).
The handshake examples use loopback sockets and finish after an encrypted exchange.
The [HTTP contract example](../crates/solti-api/examples/http_contract.rs) demonstrates bearer rejection before handler execution without opening a socket.
