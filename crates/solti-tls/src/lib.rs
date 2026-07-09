//! # solti-tls
//!
//! Shared TLS and mTLS config for Solti network crates.
//!
//! `solti-tls` gives you small builder types for `rustls` server and client configs.
//! You can point them at PEM files, or pass PEM bytes that are already in memory.
//!
//! ## Core Model
//!
//! ```text
//! PemSource::Path                 PemSource::Bytes
//!        \                       /
//!         \                     /
//!          v                   v
//!     ServerTlsConfig / ClientTlsConfig
//!          |
//!          | into_rustls_config()
//!          v
//!     rustls::ServerConfig / rustls::ClientConfig
//! ```
//!
//! A [`PemSource`] says where a PEM blob lives.
//! [`ServerTlsConfig`] describes the server certificate, key, optional client CA for mTLS, and ALPN.
//! [`ClientTlsConfig`] describes the CA used to verify the server, optional client certificate and key for mTLS, and ALPN.
//!
//! ## Main Types
//!
//! | Area      | Types                                           |
//! |-----------|-------------------------------------------------|
//! | Server    | [`ServerTlsConfig`], [`ServerTlsConfigBuilder`] |
//! | Client    | [`ClientTlsConfig`], [`ClientTlsConfigBuilder`] |
//! | PEM input | [`PemSource`]                                   |
//! | Parsing   | [`load_certs_from_pem`], [`load_key_from_pem`]  |
//! | Provider  | [`ensure_default_provider`]                     |
//! | Errors    | [`TlsError`]                                    |
//!
//! ## Quick Start
//!
//! Build a server config from files:
//!
//! ```rust,no_run
//! use solti_tls::ServerTlsConfig;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let server = ServerTlsConfig::builder()
//!     .cert_pem_file("/etc/solti/tls/server.crt")
//!     .key_pem_file("/etc/solti/tls/server.key")
//!     .with_alpn(["h2"])
//!     .build()?;
//!
//! let rustls_config = server.into_rustls_config()?;
//! # let _ = rustls_config;
//! # Ok(()) }
//! ```
//!
//! Build a client config from files:
//!
//! ```rust,no_run
//! use solti_tls::ClientTlsConfig;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let client = ClientTlsConfig::builder()
//!     .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
//!     .with_alpn(["h2"])
//!     .build()?;
//!
//! let rustls_config = client.into_rustls_config()?;
//! # let _ = rustls_config;
//! # Ok(()) }
//! ```
//!
//! ## mTLS
//!
//! On the server, call `require_client_ca_*` to require client certificates.
//! On the client, set `client_cert_*` and `client_key_*` together.
//!
//! ```rust,no_run
//! use solti_tls::{ClientTlsConfig, ServerTlsConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let server = ServerTlsConfig::builder()
//!     .cert_pem_file("/etc/solti/tls/server.crt")
//!     .key_pem_file("/etc/solti/tls/server.key")
//!     .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")
//!     .build()?;
//!
//! let client = ClientTlsConfig::builder()
//!     .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
//!     .client_cert_pem_file("/etc/solti/tls/agent.crt")
//!     .client_key_pem_file("/etc/solti/tls/agent.key")
//!     .build()?;
//!
//! # let _ = (server, client);
//! # Ok(()) }
//! ```
//!
//! ## Security Notes
//!
//! A client config verifies that the server certificate chains to the CA bundle you pass.
//! A server config with `require_client_ca_*` requires client certificates signed by that CA.
//!
//! Hostname checks happen later, at connect time.
//! Pass the real server name to `rustls`, `tonic`, or `reqwest`.
//! This crate does not use the OS trust store and does not check OCSP or CRLs.
//!
//! [`PemSource::Bytes`] may hold private keys.
//! Its `Debug` output is redacted, but the bytes are not zeroed on drop.

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
