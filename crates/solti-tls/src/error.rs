#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    #[error("PEM I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no certificates found in PEM")]
    NoCertificates,

    #[error("no private key found in PEM")]
    NoPrivateKey,

    #[error("missing required field: {0}")]
    MissingField(&'static str),

    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),

    #[error("client verifier build failed: {0}")]
    ClientVerifier(String),
}
