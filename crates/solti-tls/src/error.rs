//! # TLS error type.

/// Errors returned while loading PEM material and constructing TLS configurations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    /// Reading a [`PemSource::Path`](crate::PemSource::Path) from disk failed.
    /// The underlying [`std::io::Error`] is the source.
    #[error("PEM I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The PEM parsed but contained no `CERTIFICATE` blocks where at least one was required (a cert chain or a CA bundle).
    #[error("no certificates found in PEM")]
    NoCertificates,

    /// The PEM parsed but contained no private key (`PKCS#8`, `PKCS#1`, or `SEC1`).
    #[error("no private key found in PEM")]
    NoPrivateKey,

    /// A required builder field was absent at `build()` time, or a paired field was supplied alone (e.g. a client cert without its key).
    /// Carries the field name (`"cert"`, `"key"`, `"ca"`, `"client_cert"`, `"client_key"`).
    #[error("missing required field: {0}")]
    MissingField(&'static str),

    /// `rustls` rejected the assembled configuration - most commonly a certificate/key mismatch caught by `with_single_cert`.
    /// The [`rustls::Error`] is the source.
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),

    /// Building the mTLS client-certificate verifier failed - e.g. an empty trust anchor set or an invalid CRL.
    /// The structured [`rustls::server::VerifierBuilderError`] is preserved as the source.
    #[error("client verifier build failed: {0}")]
    ClientVerifier(#[from] rustls::server::VerifierBuilderError),
}
