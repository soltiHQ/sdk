//! Chain model errors.

use thiserror::Error;

/// Error returned while constructing or validating a chain.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ChainError {
    /// A chain field or graph invariant is invalid.
    #[error("invalid chain: {0}")]
    Invalid(String),
}

/// Convenience result type for chain model operations.
pub type ChainResult<T> = Result<T, ChainError>;
