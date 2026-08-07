//! Chain model errors.

use solti_model::ModelError;
use thiserror::Error;

/// Error returned while constructing, validating, or decoding a chain.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChainError {
    /// A chain field or graph invariant is invalid.
    #[error("invalid chain: {0}")]
    Invalid(String),

    /// A workload does not have the chain extension GVK.
    #[error(
        "expected chain workload {expected_api_version}/{expected_kind}, got {api_version}/{kind}"
    )]
    UnexpectedWorkload {
        /// Expected API group and version.
        expected_api_version: &'static str,
        /// Expected resource kind.
        expected_kind: &'static str,
        /// Actual API group and version.
        api_version: String,
        /// Actual resource kind.
        kind: String,
    },

    /// JSON conversion of the extension `spec` failed.
    #[error("invalid chain extension spec: {0}")]
    Json(#[from] serde_json::Error),

    /// The shared Solti model rejected a nested field or extension envelope.
    #[error("invalid chain model value: {0}")]
    Model(#[from] ModelError),
}

/// Convenience result type for chain model operations.
pub type ChainResult<T> = Result<T, ChainError>;
