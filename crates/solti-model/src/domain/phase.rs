//! # Task phase
//!
//! [`TaskPhase`] is the current lifecycle state recorded in [`TaskStatus`](crate::TaskStatus).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};

/// Current logical lifecycle phase of a task.
///
/// Phases describe the state visible on [`TaskStatus`](crate::TaskStatus).
/// Terminal phases record logical outcomes and do not by themselves prove
/// physical exit.
///
/// ## Example
///
/// ```
/// use solti_model::TaskPhase;
///
/// let phase: TaskPhase = "running".parse().unwrap();
/// assert!(phase.is_active());
/// assert_eq!(phase.to_string(), "running");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TaskPhase {
    /// The desired generation is pending runtime observation.
    Pending,
    /// An attempt has started.
    Running,
    /// A successful outcome was recorded.
    Succeeded,
    /// A failure outcome was recorded.
    Failed,
    /// A timeout outcome was recorded.
    Timeout,
    /// A logical cancellation or canceled admission outcome was recorded.
    Canceled,
    /// Failure retry budget exhaustion was recorded.
    Exhausted,
}

impl fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskPhase::Pending => f.write_str("pending"),
            TaskPhase::Running => f.write_str("running"),
            TaskPhase::Succeeded => f.write_str("succeeded"),
            TaskPhase::Failed => f.write_str("failed"),
            TaskPhase::Timeout => f.write_str("timeout"),
            TaskPhase::Canceled => f.write_str("canceled"),
            TaskPhase::Exhausted => f.write_str("exhausted"),
        }
    }
}

impl FromStr for TaskPhase {
    type Err = ModelError;

    /// Parses a phase name.
    ///
    /// Parsing trims whitespace and ignores ASCII case.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::UnknownTaskPhase`] for an unknown value.
    fn from_str(s: &str) -> ModelResult<Self> {
        let trimmed = s.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "pending" => Ok(TaskPhase::Pending),
            "running" => Ok(TaskPhase::Running),
            "succeeded" => Ok(TaskPhase::Succeeded),
            "failed" => Ok(TaskPhase::Failed),
            "timeout" => Ok(TaskPhase::Timeout),
            "canceled" => Ok(TaskPhase::Canceled),
            "exhausted" => Ok(TaskPhase::Exhausted),
            _ => Err(ModelError::UnknownTaskPhase(trimmed.to_string())),
        }
    }
}

impl TaskPhase {
    /// Returns whether the phase is terminal.
    ///
    /// A later attempt may still start under [`RestartPolicy`](crate::RestartPolicy).
    /// Resource reconciliation may also refine or replace a terminal outcome.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskPhase;
    ///
    /// assert!(TaskPhase::Succeeded.is_terminal());
    /// assert!(!TaskPhase::Running.is_terminal());
    /// ```
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskPhase::Succeeded
                | TaskPhase::Failed
                | TaskPhase::Timeout
                | TaskPhase::Canceled
                | TaskPhase::Exhausted
        )
    }

    /// Returns whether the phase is pending or running.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskPhase;
    ///
    /// assert!(TaskPhase::Pending.is_active());
    /// assert!(!TaskPhase::Failed.is_active());
    /// ```
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(self, TaskPhase::Pending | TaskPhase::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_and_terminal_classification_covers_every_phase() {
        for phase in [TaskPhase::Pending, TaskPhase::Running] {
            assert!(phase.is_active());
            assert!(!phase.is_terminal());
        }
        for phase in [
            TaskPhase::Succeeded,
            TaskPhase::Failed,
            TaskPhase::Timeout,
            TaskPhase::Canceled,
            TaskPhase::Exhausted,
        ] {
            assert!(!phase.is_active());
            assert!(phase.is_terminal());
        }
    }

    #[test]
    fn display_parse_and_serde_roundtrip_every_phase() {
        let cases = [
            ("pending", TaskPhase::Pending),
            ("running", TaskPhase::Running),
            ("succeeded", TaskPhase::Succeeded),
            ("failed", TaskPhase::Failed),
            ("timeout", TaskPhase::Timeout),
            ("canceled", TaskPhase::Canceled),
            ("exhausted", TaskPhase::Exhausted),
        ];
        for (wire, phase) in cases {
            assert_eq!(phase.to_string(), wire);
            assert_eq!(wire.parse::<TaskPhase>().unwrap(), phase);
            let json = serde_json::to_string(&phase).unwrap();
            assert_eq!(serde_json::from_str::<TaskPhase>(&json).unwrap(), phase);
        }
    }

    #[test]
    fn parsing_normalizes_case_and_whitespace_and_rejects_unknown_values() {
        assert_eq!("RUNNING".parse::<TaskPhase>().unwrap(), TaskPhase::Running);
        assert_eq!(
            "  Succeeded  ".parse::<TaskPhase>().unwrap(),
            TaskPhase::Succeeded
        );
        let err = "bogus".parse::<TaskPhase>().unwrap_err();
        assert!(matches!(err, ModelError::UnknownTaskPhase(_)));
    }
}
