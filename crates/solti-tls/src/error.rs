//! # TLS errors

use std::path::PathBuf;

/// The purpose of PEM material in TLS settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PemRole {
    /// Certificate chain presented by a server.
    ServerCertificate,
    /// Private key used by a server.
    ServerPrivateKey,
    /// Roots used by a client to verify a server.
    ServerTrustRoots,
    /// Certificate chain presented by a client during mutual TLS.
    ClientCertificate,
    /// Private key used by a client during mutual TLS.
    ClientPrivateKey,
    /// Roots used by a server to verify mutual TLS clients.
    ClientTrustRoots,
}

impl std::fmt::Display for PemRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ServerCertificate => "server certificate chain",
            Self::ServerPrivateKey => "server private key",
            Self::ServerTrustRoots => "server trust roots",
            Self::ClientCertificate => "client certificate chain",
            Self::ClientPrivateKey => "client private key",
            Self::ClientTrustRoots => "client trust roots",
        })
    }
}

/// Errors returned while loading and validating TLS material.
///
/// Errors are grouped by where they happen:
///
/// | Step                   | Variants                                                                                                              |
/// |------------------------|-----------------------------------------------------------------------------------------------------------------------|
/// | Read a file            | [`TlsError::ReadPem`]                                                                                                 |
/// | Parse PEM              | [`TlsError::InvalidPem`], [`TlsError::NoCertificates`], [`TlsError::NoPrivateKey`], [`TlsError::MultiplePrivateKeys`] |
/// | Add a trust root       | [`TlsError::InvalidCertificate`]                                                                                      |
/// | Build TLS settings     | [`TlsError::Configuration`], [`TlsError::ClientVerifier`]                                                             |
///
/// Match with a wildcard arm because this enum is non-exhaustive.
///
/// ```
/// use solti_tls::TlsError;
///
/// fn is_file_error(error: &TlsError) -> bool {
///     match error {
///         TlsError::ReadPem { .. } => true,
///         _ => false,
///     }
/// }
/// # let _ = is_file_error;
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    /// Reading a configured PEM file failed.
    #[error("failed to read {role} PEM from {path}: {source}")]
    ReadPem {
        /// Purpose of the PEM input.
        role: PemRole,
        /// Exact file that could not be read.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },

    /// A PEM block was syntactically invalid for its role.
    #[error("invalid {role} PEM: {source}")]
    InvalidPem {
        /// Purpose of the PEM input.
        role: PemRole,
        /// Underlying PEM parser error.
        #[source]
        source: rustls::pki_types::pem::Error,
    },

    /// A certificate or trust-root input contained no certificate blocks.
    #[error("no certificates found in {role} PEM")]
    NoCertificates {
        /// Purpose of the PEM input.
        role: PemRole,
    },

    /// A private-key input contained no supported key block.
    #[error("no private key found in {role} PEM")]
    NoPrivateKey {
        /// Purpose of the PEM input.
        role: PemRole,
    },

    /// A private-key input contained more than one key.
    #[error("multiple private keys found in {role} PEM")]
    MultiplePrivateKeys {
        /// Purpose of the PEM input.
        role: PemRole,
    },

    /// A certificate could not be added to the rustls trust store for its role.
    #[error("invalid certificate in {role}: {source}")]
    InvalidCertificate {
        /// Purpose of the certificate input.
        role: PemRole,
        /// Underlying rustls error.
        #[source]
        source: rustls::Error,
    },

    /// rustls rejected a certificate/private-key identity.
    #[error("failed to configure {context}: {source}")]
    Configuration {
        /// Identity named in the error message.
        context: &'static str,
        /// Underlying rustls error.
        #[source]
        source: rustls::Error,
    },

    /// Building the mandatory mutual TLS client verifier failed.
    #[error("failed to build client certificate verifier: {source}")]
    ClientVerifier {
        /// Underlying verifier builder error.
        #[source]
        source: rustls::server::VerifierBuilderError,
    },
}
