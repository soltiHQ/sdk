//! Error types.

use std::fmt;

use thiserror::Error;

use solti_model::{TaskId, Uid};
use solti_runner::RouterError;

/// One failed resource write precondition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WritePreconditionViolation {
    /// The current resource has a different UID.
    Uid {
        /// UID required by the caller.
        expected: Uid,
        /// UID stored on the current resource.
        actual: Uid,
    },
    /// The current resource has a different resource version.
    ResourceVersion {
        /// Resource version required by the caller.
        expected: String,
        /// Resource version stored on the current resource.
        actual: String,
    },
}

impl fmt::Display for WritePreconditionViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uid { expected, actual } => {
                write!(formatter, "uid expected `{expected}`, current `{actual}`")
            }
            Self::ResourceVersion { expected, actual } => write!(
                formatter,
                "resourceVersion expected `{expected}`, current `{actual}`"
            ),
        }
    }
}

/// Details of an optimistic-concurrency conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteConflict {
    name: TaskId,
    violations: Vec<WritePreconditionViolation>,
}

impl WriteConflict {
    pub(crate) fn new(name: TaskId, violations: Vec<WritePreconditionViolation>) -> Self {
        debug_assert!(!violations.is_empty());
        Self { name, violations }
    }

    /// Conflicting task name.
    pub fn name(&self) -> &TaskId {
        &self.name
    }

    /// Failed preconditions.
    pub fn violations(&self) -> &[WritePreconditionViolation] {
        &self.violations
    }
}

impl fmt::Display for WriteConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "write precondition failed for Task `{}`: ",
            self.name
        )?;
        for (index, violation) in self.violations.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{violation}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WriteConflict {}

/// Error type returned by every fallible operation in solti-core.
///
/// The enum is `#[non_exhaustive]`; match with a wildcard arm.
///
/// ## Example
///
/// ```
/// use solti_core::CoreError;
///
/// fn is_missing(err: &CoreError) -> bool {
///     match err {
///         CoreError::NotFound(_) => true,
///         _ => false,
///     }
/// }
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// The in-memory state store could not initialize its identity.
    #[error("state initialization failed: {0}")]
    StateInitialization(#[source] solti_model::ModelError),

    /// The supervisor has started shutting down and no longer accepts desired-state writes.
    #[error("supervisor is shutting down")]
    ShuttingDown,

    /// A taskvisor runtime failure during submit, cancel, or shutdown.
    ///
    /// Carries the typed [`taskvisor::Error`] as the source; `op` is a stable
    /// label for the failed operation, such as `"prepare"`, `"submit"`,
    /// `"cancel"`, or `"shutdown"`.
    #[error("supervisor {op} failed: {source}")]
    Supervisor {
        /// The operation that failed.
        op: &'static str,
        /// The underlying taskvisor error.
        #[source]
        source: taskvisor::Error,
    },

    /// A retained Task resource already owns this metadata.name.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// The referenced task does not exist.
    #[error("task not found: {0}")]
    NotFound(String),

    /// One or more write preconditions do not match the current resource.
    #[error(transparent)]
    Conflict(WriteConflict),

    /// A model-to-taskvisor policy mapping failure:
    /// a `#[non_exhaustive]` model enum carried a variant with no taskvisor equivalent.
    #[error("mapping error: {0}")]
    Mapping(String),

    /// Runner routing or task construction failed.
    /// Wraps [`solti_runner::RouterError`].
    #[error("runner error: {0}")]
    Runner(#[from] RouterError),

    /// The submitted spec failed validation.
    /// Wraps [`solti_model::ModelError`].
    #[error("invalid spec: {0}")]
    InvalidSpec(#[from] solti_model::ModelError),
}

impl CoreError {
    /// Wrap a taskvisor failure with the operation label.
    ///
    /// Accepts both [`taskvisor::RuntimeError`] and [`taskvisor::ControllerError`]
    /// through the umbrella [`taskvisor::Error`].
    pub(crate) fn supervisor(op: &'static str, e: impl Into<taskvisor::Error>) -> Self {
        Self::Supervisor {
            op,
            source: e.into(),
        }
    }
}
