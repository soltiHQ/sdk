//! # Restart policy.
//!
//! [`RestartPolicy`] controls when a task is restarted after completion or failure.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Determines whether a task should be automatically restarted after completion or failure.
///
/// | Variant     | Behaviour                                                    |
/// |-------------|--------------------------------------------------------------|
/// | `Never`     | Do not restart under any circumstances                       |
/// | `OnFailure` | Restart only when the task ends with an error                |
/// | `Always`    | Restart unconditionally (immediate or periodic via interval) |
///
/// `Always { interval_ms: None }` restarts immediately;
/// `Always { interval_ms: Some(N) }` waits N ms between runs (periodic task).
///
/// Cancellation (via controller or shutdown) is **not** treated as failure and will not trigger a restart.
///
/// ## Also
///
/// - [`BackoffPolicy`](super::BackoffPolicy) delay between restart attempts.
/// - [`TaskSpec`](crate::TaskSpec) carries `restart` as a field.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Never restart the task.
    #[default]
    Never,
    /// Restart the task only if it failed (non-zero exit, error, panic, etc.).
    OnFailure,
    /// Always restart after completion.
    #[serde(rename_all = "camelCase")]
    Always {
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
}

impl RestartPolicy {
    /// Create an Always policy without interval (immediate restart).
    pub const fn always() -> Self {
        RestartPolicy::Always { interval_ms: None }
    }

    /// Create an Always policy with periodic interval.
    pub const fn periodic(interval_ms: u64) -> Self {
        RestartPolicy::Always {
            interval_ms: Some(interval_ms),
        }
    }
}

impl FromStr for RestartPolicy {
    type Err = ModelError;

    fn from_str(s: &str) -> ModelResult<Self> {
        let original = s.trim();
        if original.is_empty() {
            return Ok(RestartPolicy::Never);
        }

        let (head, rest) = match original.find(':') {
            Some(pos) => (&original[..pos], Some(original[pos + 1..].trim())),
            None => (original, None),
        };

        if head.eq_ignore_ascii_case("never") {
            Ok(RestartPolicy::Never)
        } else if head.eq_ignore_ascii_case("on-failure") || head.eq_ignore_ascii_case("failure") {
            Ok(RestartPolicy::OnFailure)
        } else if head.eq_ignore_ascii_case("always") {
            let interval_ms = match rest {
                None | Some("") => None,
                Some(v) => {
                    let v = v.parse::<u64>().map_err(|_| {
                        ModelError::UnknownRestart(format!(
                            "invalid interval in '{}': must be u64",
                            original
                        ))
                    })?;
                    Some(v)
                }
            };
            Ok(RestartPolicy::Always { interval_ms })
        } else {
            Err(ModelError::UnknownRestart(original.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RestartPolicy;
    use crate::error::ModelError;
    use std::str::FromStr;

    #[test]
    fn parse_never_and_empty() {
        assert_eq!(RestartPolicy::from_str("").unwrap(), RestartPolicy::Never);
        assert_eq!(
            RestartPolicy::from_str("never").unwrap(),
            RestartPolicy::Never
        );
        assert_eq!(
            RestartPolicy::from_str("  NeVeR  ").unwrap(),
            RestartPolicy::Never
        );
    }

    #[test]
    fn parse_on_failure() {
        assert_eq!(
            RestartPolicy::from_str("on-failure").unwrap(),
            RestartPolicy::OnFailure
        );
        assert_eq!(
            RestartPolicy::from_str("failure").unwrap(),
            RestartPolicy::OnFailure
        );
        assert_eq!(
            RestartPolicy::from_str("  Failure ").unwrap(),
            RestartPolicy::OnFailure
        );
    }

    #[test]
    fn parse_always_immediate() {
        assert_eq!(
            RestartPolicy::from_str("always").unwrap(),
            RestartPolicy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartPolicy::from_str("  ALWAYS  ").unwrap(),
            RestartPolicy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartPolicy::from_str("always:").unwrap(),
            RestartPolicy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartPolicy::from_str("always:   ").unwrap(),
            RestartPolicy::Always { interval_ms: None }
        );
    }

    #[test]
    fn parse_always_with_interval() {
        assert_eq!(
            RestartPolicy::from_str("always:1000").unwrap(),
            RestartPolicy::Always {
                interval_ms: Some(1000)
            }
        );
        assert_eq!(
            RestartPolicy::from_str(" Always:  60000 ").unwrap(),
            RestartPolicy::Always {
                interval_ms: Some(60000)
            }
        );
    }

    #[test]
    fn parse_always_invalid_interval() {
        let err = RestartPolicy::from_str("always:not-a-number").unwrap_err();
        assert!(matches!(err, ModelError::UnknownRestart(_)));
    }

    #[test]
    fn parse_unknown_head_fails() {
        let err = RestartPolicy::from_str("random").unwrap_err();
        assert!(matches!(err, ModelError::UnknownRestart(_)));
    }
}
