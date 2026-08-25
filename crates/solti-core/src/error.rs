//! # Core errors
//!
//! [`CoreError`] covers desired writes, runner preparation, and Taskvisor operations.
//! [`WriteConflict`] describes failed optimistic concurrency checks.
//!
//! Collection reads use [`CollectionError`](crate::CollectionError).
//! Checked configuration uses [`ConfigError`](crate::ConfigError).

use std::{fmt, time::Duration};

use thiserror::Error;

use solti_model::{TaskId, Uid};
use solti_runner::RouterError;

/// One failed write precondition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WritePreconditionViolation {
    /// The stored resource has a different UID.
    Uid {
        /// Required UID.
        expected: Uid,
        /// Stored UID.
        actual: Uid,
    },
    /// The stored resource has a different resource version.
    ResourceVersion {
        /// Required resource version.
        expected: String,
        /// Stored resource version.
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

/// Optimistic concurrency conflict details.
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

    /// Returns the conflicting task name.
    pub fn name(&self) -> &TaskId {
        &self.name
    }

    /// Returns every failed precondition.
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

/// Error from a desired write, runner preparation, or Taskvisor operation.
///
/// This enum is non-exhaustive.
/// Match it with a wildcard arm.
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
    /// State could not initialize its resource-version identity.
    #[error("state initialization failed: {0}")]
    StateInitialization(#[source] solti_model::ModelError),

    /// The core-owned persistence delivery worker could not start.
    #[error("persistence initialization failed: {0}")]
    PersistenceInitialization(#[source] std::io::Error),

    /// Taskvisor could not build the stopped supervisor.
    #[error("supervisor initialization failed: {0}")]
    SupervisorInitialization(#[source] taskvisor::BuildError),

    /// Shutdown has started.
    ///
    /// Desired-state writes are no longer accepted.
    #[error("supervisor is shutting down")]
    ShuttingDown,

    /// The caller-provided shutdown deadline elapsed while SDK-owned work was
    /// still draining.
    ///
    /// The accepted shutdown coordinator remains owned by the SDK and
    /// continues cleanup after this error is returned.
    #[error("SDK shutdown did not drain within {timeout:?}")]
    ShutdownTimedOut {
        /// Deadline supplied by the caller.
        timeout: Duration,
    },

    /// The SDK-owned shutdown coordinator stopped before publishing an
    /// outcome.
    #[error("SDK shutdown coordinator stopped unexpectedly")]
    ShutdownCoordinatorStopped,

    /// A Taskvisor operation failed.
    ///
    /// `op` is a stable operation label.
    /// Known labels are `"start"`, `"prepare"`, `"submit"`, `"cancel"`, and `"shutdown"`.
    #[error("supervisor {op} failed: {source}")]
    Supervisor {
        /// Stable operation label.
        op: &'static str,
        /// Taskvisor error.
        #[source]
        source: taskvisor::Error,
    },

    /// A retained task already owns the name.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// The state cannot retain another task.
    #[error("retained task limit reached: {limit}")]
    RetainedTaskLimitReached {
        /// Configured task count limit.
        limit: usize,
    },

    /// A desired write would exceed the retained TaskManifest byte budget.
    #[error(
        "retained task manifest byte limit exceeded: current {current} bytes, requested {requested} bytes, limit {limit} bytes"
    )]
    RetainedTaskManifestByteLimitExceeded {
        /// Caller-owned manifest bytes retained before this write.
        current: usize,
        /// Additional caller-owned manifest bytes required by this write.
        requested: usize,
        /// Configured aggregate byte limit.
        limit: usize,
    },

    /// The task does not exist or is hidden by an adapter predicate.
    #[error("task not found: {0}")]
    NotFound(String),

    /// Write preconditions do not match the stored task.
    #[error(transparent)]
    Conflict(WriteConflict),

    /// A model policy has no Taskvisor mapping.
    #[error("mapping error: {0}")]
    Mapping(String),

    /// Runner selection or task construction failed.
    ///
    /// Contains the [`solti_runner::RouterError`].
    #[error("runner error: {0}")]
    Runner(#[from] RouterError),

    /// The submitted model or workload path is invalid.
    ///
    /// Contains the [`solti_model::ModelError`].
    #[error("invalid spec: {0}")]
    InvalidSpec(#[from] solti_model::ModelError),
}

impl CoreError {
    /// Wraps a Taskvisor failure with an operation label.
    ///
    /// Accepts runtime and controller failures through [`taskvisor::Error`].
    pub(crate) fn supervisor(op: &'static str, e: impl Into<taskvisor::Error>) -> Self {
        Self::Supervisor {
            op,
            source: e.into(),
        }
    }
}
