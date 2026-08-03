//! # solti-tls
//!
//! Shared TLS and mTLS configuration for Solti transports.
//! It loads PEM and uses `rustls` to check the configuration material.
//! Callers own connections and application protocols.
//!
//! ## Start Here
//!
//! 1. Create a [`TlsIdentity`] from a certificate chain and private key.
//! 2. Create [`ServerTlsConfig`] or [`ClientTlsConfig`].
//! 3. Enable mutual TLS when needed; use [`ServerTlsConfig::require_client_auth`] and [`ClientTlsConfig::with_identity`].
//! 4. Build `rustls` or load validated PEM for an adapter.
//!
//! ## Flow
//!
//! ```text
//! PemSource + PrivateKeySource ──► TlsIdentity ─┐
//!                                               ├──► ServerTlsConfig / ClientTlsConfig
//! PemSource ─────────────────────► TrustRoots ──┘               │
//!                                                               ├──► into_rustls_config()
//!                                                               │         └──► rustls config
//!                                                               └──► load()
//!                                                                         └──► validated PEM
//! ```
//!
//! ## Client and Server
//!
//! | Side                | Required material                        | Optional mTLS material                |
//! |---------------------|------------------------------------------|---------------------------------------|
//! | [`ServerTlsConfig`] | Server [`TlsIdentity`]                   | [`TrustRoots`] used to verify clients |
//! | [`ClientTlsConfig`] | [`TrustRoots`] used to verify the server | Client [`TlsIdentity`]                |
//!
//! ## Loading
//!
//! Constructors do not read files.
//! File sources are read by `load()` or `into_rustls_config()`.
//! Both methods validate the material with `rustls`.
//!
//! `load()` returns the original PEM after validation.
//! `into_rustls_config()` returns the built `rustls` configuration.
//! [`TlsError`] reports the failed stage.
//!
//! ## ALPN
//!
//! - `into_rustls_config()` leaves `alpn_protocols` empty.
//! - HTTP, gRPC, or another transport sets ALPN.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let identity = TlsIdentity::from_pem_files(
//!     "/etc/solti/tls/server.crt",
//!     "/etc/solti/tls/server.key",
//! );
//! let server = ServerTlsConfig::new(identity)
//!     .require_client_auth(TrustRoots::from_pem_file(
//!         "/etc/solti/tls/clients-ca.crt",
//!     ));
//!
//! let mut rustls = server.into_rustls_config()?;
//! rustls.alpn_protocols = vec![b"h2".to_vec()];
//! # Ok(())
//! # }
//! ```
//!
//! ## Main Types
//!
//! | Type                      | Purpose                                         |
//! |---------------------------|-------------------------------------------------|
//! | [`TlsIdentity`]           | Certificate chain and private key               |
//! | [`TrustRoots`]            | Certificates used to verify the other side      |
//! | [`ServerTlsConfig`]       | Server TLS and mTLS settings                    |
//! | [`ClientTlsConfig`]       | Client TLS and mTLS settings                    |
//! | [`LoadedTlsIdentity`]     | Loaded identity PEM for an adapter              |
//! | [`LoadedServerTlsConfig`] | Loaded server PEM for an adapter                |
//! | [`LoadedClientTlsConfig`] | Loaded client PEM for an adapter                |
//! | [`PemSource`]             | Certificate or trust-root PEM source            |
//! | [`PrivateKeySource`]      | Private-key PEM source                          |
//! | [`TlsError`]              | Loading and validation errors                   |
//!
//! ## Security
//!
//! - `into_rustls_config()` does not check a server name; `rustls` checks it during the client handshake.
//! - [`ClientTlsConfig`] trusts only the configured roots; operating-system roots are not added.
//! - [`ServerTlsConfig::require_client_auth`] makes client certificates mandatory.
//! - A TLS library or adapter may keep its own copy of a private key.
//! - Private-key buffers owned by this crate are zeroized on drop.
//! - OCSP and CRL checks are not configured.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(doctest)]
/// Compiles runnable Rust code blocks in `README.md` as doctests.
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod client;
pub use client::{ClientTlsConfig, LoadedClientTlsConfig};

mod server;
pub use server::{LoadedServerTlsConfig, ServerTlsConfig};

mod identity;
pub use identity::{LoadedTlsIdentity, TlsIdentity};

mod source;
pub use source::{PemSource, PrivateKeySource};

mod error;
pub use error::{PemRole, TlsError};

mod roots;
pub use roots::TrustRoots;

mod pem;
mod provider;
