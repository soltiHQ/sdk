//! # solti-tls
//!
//! Shared TLS and mutual TLS configuration for Solti transports.
//!
//! Use it to define certificates, private keys, and trust roots once.
//! A transport can build rustls directly or load validated PEM from the same configuration.
//!
//! ## Start Here
//!
//! 1. Create a [`TlsIdentity`] from a certificate chain and private key.
//! 2. Create [`ServerTlsConfig`] or [`ClientTlsConfig`].
//! 3. Enable mutual TLS with [`ServerTlsConfig::require_client_auth`] and [`ClientTlsConfig::with_identity`].
//! 4. Call `into_rustls_config()` for rustls or `load()` for a transport adapter.
//!
//! ```text
//! PemSource + PrivateKeySource ──> TlsIdentity ──────────┐
//!                                                        │
//! PemSource ─────────────────────> TrustRoots ───────────┤
//!                                                        ▼
//!                                       ServerTlsConfig / ClientTlsConfig
//!                                              │                  │
//!                                              ▼                  ▼
//!                                  into_rustls_config()          load()
//!                                              │                  │
//!                                              ▼                  ▼
//!                                           rustls       transport adapter
//! ```
//!
//! ## Client and Server
//!
//! | Side                | Required material                        | Optional mTLS material                |
//! |---------------------|------------------------------------------|---------------------------------------|
//! | [`ServerTlsConfig`] | Server [`TlsIdentity`]                   | [`TrustRoots`] used to verify clients |
//! | [`ClientTlsConfig`] | [`TrustRoots`] used to verify the server | Client [`TlsIdentity`]                |
//!
//! ## Certificate and Key
//!
//! A [`TlsIdentity`] always contains both a certificate chain and its private key.
//! It cannot contain only one of them.
//!
//! ## When Files Are Read
//!
//! Constructors do not read files.
//! File sources are read by `load()` or `into_rustls_config()`.
//!
//! Both methods parse the PEM, build the required rustls verifiers, and check certificate/private-key pairs.
//! [`TlsError`] reports what failed; file errors also include the path.
//!
//! ## ALPN
//!
//! - Adapters can use [`LoadedServerTlsConfig`] or [`LoadedClientTlsConfig`] from `load()`.
//! - `into_rustls_config()` leaves `alpn_protocols` empty.
//! - The HTTP, gRPC, or other transport sets ALPN.
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
//! | [`ServerTlsConfig`]       | Server TLS and mutual TLS settings              |
//! | [`ClientTlsConfig`]       | Client TLS and mutual TLS settings              |
//! | [`LoadedTlsIdentity`]     | Loaded identity PEM for an adapter              |
//! | [`LoadedServerTlsConfig`] | Loaded server PEM for an adapter                |
//! | [`LoadedClientTlsConfig`] | Loaded client PEM for an adapter                |
//! | [`PemSource`]             | Certificate or trust-root PEM source            |
//! | [`PrivateKeySource`]      | Private-key PEM source                          |
//! | [`TlsError`]              | Loading and validation errors                   |
//!
//! ## Security
//!
//! - [`ClientTlsConfig`] trusts only the configured roots. Operating-system roots are not added.
//! - [`ServerTlsConfig::require_client_auth`] makes client certificates mandatory.
//! - A TLS library or adapter may keep its own copy of a private key.
//! - Private-key buffers owned by this crate are zeroized on drop.
//! - The server name is checked when the client connects.
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
