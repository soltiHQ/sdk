//! # solti-tls
//!
//! Shared TLS / mTLS configuration for Solti network-facing crates.

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
