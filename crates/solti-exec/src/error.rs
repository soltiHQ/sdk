//! # Error types.

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExecError {
    /// A runner with this name is already registered in the router.
    #[error("duplicate runner detected: runner with name '{name}' is already registered")]
    DuplicateRunner {
        /// The conflicting runner name.
        name: String,
    },

    /// Backend configuration failed validation (zero limits, fail-open security policy, unenforceable confinement, …).
    #[error("invalid runner configuration: {0}")]
    InvalidRunnerConfig(String),

    /// The task spec was malformed (e.g. an empty command).
    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    /// An unexpected internal error. Reserved as a forward-compatibility escape hatch; not constructed on any current path.
    #[error("internal error: {0}")]
    Internal(String),

    /// An OS-level I/O failure (spawn, cgroup/tempfile setup, …).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
