//! # solti-tls
//!
//! Shared TLS / mTLS configuration for Solti network-facing crates.
//!
//! Builders hold *intent*: a [`PemSource`] (filesystem path or in-memory PEM bytes) for each cert/key.
//! Defer all I/O and parsing to [`ServerTlsConfig::into_rustls_config`] / [`ClientTlsConfig::into_rustls_config`], which produce a ready [`rustls::ServerConfig`] / [`rustls::ClientConfig`].
//!
//! ## Architecture
//!
//! ```text
//!   ┌──────────────────────┐        ┌──────────────────────┐
//!   │  PemSource::Path     │        │  PemSource::Bytes    │   intent: where PEM lives
//!   │  (read at I/O time)  │        │  (in-memory buffer)  │
//!   └──────────┬───────────┘        └──────────┬───────────┘
//!              └───────────────┬───────────────┘
//!                              ▼
//!         ┌───────────────────────────────────────────────┐
//!         │  ServerTlsConfig / ClientTlsConfig            │   built via *Builder
//!         │   cert · key · client_ca/ca · alpn            │   (pure, cloneable intent)
//!         └───────────────────────┬───────────────────────┘
//!                                 │ into_rustls_config()
//!                                 │   1. ensure_default_provider()  (install ring once)
//!                                 │   2. PemSource::read()          (I/O happens here)
//!                                 │   3. load_certs_from_pem / load_key_from_pem
//!                                 │   4. RootCertStore + (mTLS) WebPki*Verifier
//!                                 │   5. apply ALPN
//!                                 ▼
//!         ┌───────────────────────────────────────────────┐
//!         │  rustls::ServerConfig / rustls::ClientConfig  │
//!         └───────────────────────────────────────────────┘
//! ```
//!
//! No I/O, provider install, or parsing happens until `into_rustls_config()` is called; the builders are pure intent.
//!
//! ## Features
//!
//! | Area            | Description                                                         | Key types                                       |
//! |-----------------|---------------------------------------------------------------------|-------------------------------------------------|
//! | **PEM sources** | Path-on-disk or in-memory bytes; read lazily at config-build time.  | [`PemSource`]                                   |
//! | **Server TLS**  | Server cert/key, optional client-CA (mTLS), ALPN.                   | [`ServerTlsConfig`], [`ServerTlsConfigBuilder`] |
//! | **Client TLS**  | Trust roots (CA), optional client cert/key (mTLS), ALPN.            | [`ClientTlsConfig`], [`ClientTlsConfigBuilder`] |
//! | **Parsing**     | Pure PEM→DER helpers for certs and the first private key.           | [`load_certs_from_pem`], [`load_key_from_pem`]  |
//! | **Provider**    | Idempotent process-wide installation of the `ring` crypto provider. | [`ensure_default_provider`]                     |
//! | **Errors**      | One `#[non_exhaustive]` enum across I/O, parse, and `rustls`.       | [`TlsError`]                                    |
//!
//! ## Security model
//!
//! **What IS verified**
//! - *Server chain*: a client built from [`ClientTlsConfig`] verifies the server's certificate chains to the CA bundle you pass to `ca(..)` (`rustls`' `WebPkiServerVerifier`).
//!   Trust roots come **only** from your PEM - there is no OS / `rustls-native-certs` trust integration.
//! - *Client chain (mTLS)*: when `require_client_ca(..)` is set, the server demands and verifies a client cert chaining to that CA.
//!   Client auth is then **mandatory**: connections without a valid client cert are rejected at the handshake.
//!
//! **What is NOT verified here (the caller's responsibility)**
//! - *Hostname / SAN*: `into_rustls_config()` does not check the server hostname.
//!   SAN/identity matching happens when you call `TlsConnector::connect(server_name, ..)`: you must pass the correct [`ServerName`](rustls::pki_types::ServerName).
//!   A wrong or placeholder name silently disables identity checking even though the chain still validates.
//! - *Revocation*: no OCSP / CRL / stapling; a revoked-but-unexpired cert is accepted.
//!   Short cert lifetimes are the intended mitigation.
//! - *TLS versions & suites*: `rustls` safe defaults only (the workspace enables TLS 1.2 + 1.3); no explicit minimum-version or cipher policy.
//! - *Crypto provider*: `ring` is installed via [`ensure_default_provider`]; `aws-lc-rs` is not selectable (install your own provider first if needed).
//!
//! ## Example
//!
//! ```rust
//! use solti_tls::ServerTlsConfig;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Use real PEM files in production; placeholder bytes keep this hermetic.
//! let cfg = ServerTlsConfig::builder()
//!     .cert_pem_bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec())
//!     .key_pem_bytes(b"-----BEGIN PRIVATE KEY-----\n...".to_vec())
//!     .with_alpn(["h2"])              // gRPC; ["h2", "http/1.1"] for HTTP
//!     .build()?;                      // validates required fields, no I/O yet
//!
//! assert_eq!(cfg.alpn, vec![b"h2".to_vec()]);
//! // cfg.into_rustls_config()? would now read + parse the PEM into a
//! // rustls::ServerConfig (requires real cert/key material).
//! # Ok(()) }
//! ```
//!
//! ## Also
//! - [`ServerTlsConfig`] / [`ClientTlsConfig`] - the two entry points.
//! - [`PemSource`] - path-vs-bytes intent shared by both.
//! - [`TlsError`] - every failure mode of `build` / `into_rustls_config`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::TlsError;

mod source;
pub use source::PemSource;

mod provider;
pub use provider::ensure_default_provider;

mod pem;
pub use pem::{load_certs_from_pem, load_key_from_pem};

mod server;
pub use server::{ServerTlsConfig, ServerTlsConfigBuilder};

mod client;
pub use client::{ClientTlsConfig, ClientTlsConfigBuilder};
