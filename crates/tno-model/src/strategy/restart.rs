use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Determines whether a task should be automatically restarted after it completes or fails.
///
/// The restart strategy operates at the supervisor layer and is applied to each task instance independently.
/// If combined with a backoff policy, restarts will be delayed according to the configured backoff + jitter.
///
/// Strategies:
/// - `Never`: Do not restart the task under any circumstances.
/// - `OnFailure`: Restart only when the task ends with an error.
/// - `Always`: Restart unconditionally after completion or failure.
///   - `interval_ms: None` → restart immediately
///   - `interval_ms: Some(N)` → periodic task, wait N milliseconds between runs
///
/// Restart behavior is evaluated after each task execution cycle.
/// If a task is canceled (via controller or shutdown), it is **not** considered a failure
/// and will not be restarted unless explicitly treated as such by the runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RestartStrategy {
    /// Never restart the task.
    Never,
    /// Restart the task only if it failed (non-zero exit, error, panic, etc.).
    OnFailure,
    /// Always restart after completion.
    ///
    /// If `interval_ms` is provided, the task becomes periodic and waits
    /// the specified duration before the next cycle.
    #[serde(rename_all = "camelCase")]
    Always {
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
}

impl RestartStrategy {
    /// Create an Always policy without interval (immediate restart).
    pub const fn always() -> Self {
        RestartStrategy::Always { interval_ms: None }
    }

    /// Create an Always policy with periodic interval.
    pub const fn periodic(interval_ms: u64) -> Self {
        RestartStrategy::Always {
            interval_ms: Some(interval_ms),
        }
    }
}

impl Default for RestartStrategy {
    fn default() -> Self {
        RestartStrategy::OnFailure
    }
}

impl FromStr for RestartStrategy {
    type Err = ModelError;

    fn from_str(s: &str) -> ModelResult<Self> {
        let original = s.trim();
        if original.is_empty() {
            return Ok(RestartStrategy::Never);
        }

        let lower = original.to_ascii_lowercase();
        let mut parts = lower.splitn(2, ':');
        let head = parts.next().unwrap();

        match head {
            "never" => Ok(RestartStrategy::Never),
            "on-failure" | "failure" => Ok(RestartStrategy::OnFailure),
            "always" => {
                let interval_ms = match parts.next() {
                    None => None,
                    Some(rest) => {
                        let rest = rest.trim();
                        if rest.is_empty() {
                            None
                        } else {
                            let v = rest.parse::<u64>().map_err(|_| {
                                ModelError::UnknownRestart(format!(
                                    "invalid interval in '{}': must be u64",
                                    original
                                ))
                            })?;
                            Some(v)
                        }
                    }
                };
                Ok(RestartStrategy::Always { interval_ms })
            }
            _ => Err(ModelError::UnknownRestart(original.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RestartStrategy;
    use crate::error::ModelError;
    use std::str::FromStr;

    #[test]
    fn parse_never_and_empty() {
        assert_eq!(
            RestartStrategy::from_str("").unwrap(),
            RestartStrategy::Never
        );
        assert_eq!(
            RestartStrategy::from_str("never").unwrap(),
            RestartStrategy::Never
        );
        assert_eq!(
            RestartStrategy::from_str("  NeVeR  ").unwrap(),
            RestartStrategy::Never
        );
    }

    #[test]
    fn parse_on_failure() {
        assert_eq!(
            RestartStrategy::from_str("on-failure").unwrap(),
            RestartStrategy::OnFailure
        );
        assert_eq!(
            RestartStrategy::from_str("failure").unwrap(),
            RestartStrategy::OnFailure
        );
        assert_eq!(
            RestartStrategy::from_str("  Failure ").unwrap(),
            RestartStrategy::OnFailure
        );
    }

    #[test]
    fn parse_always_immediate() {
        assert_eq!(
            RestartStrategy::from_str("always").unwrap(),
            RestartStrategy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartStrategy::from_str("  ALWAYS  ").unwrap(),
            RestartStrategy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartStrategy::from_str("always:").unwrap(),
            RestartStrategy::Always { interval_ms: None }
        );
        assert_eq!(
            RestartStrategy::from_str("always:   ").unwrap(),
            RestartStrategy::Always { interval_ms: None }
        );
    }

    #[test]
    fn parse_always_with_interval() {
        assert_eq!(
            RestartStrategy::from_str("always:1000").unwrap(),
            RestartStrategy::Always {
                interval_ms: Some(1000)
            }
        );
        assert_eq!(
            RestartStrategy::from_str(" Always:  60000 ").unwrap(),
            RestartStrategy::Always {
                interval_ms: Some(60000)
            }
        );
    }

    #[test]
    fn parse_always_invalid_interval() {
        let err = RestartStrategy::from_str("always:not-a-number").unwrap_err();
        assert!(matches!(err, ModelError::UnknownRestart(_)));
    }

    #[test]
    fn parse_unknown_head_fails() {
        let err = RestartStrategy::from_str("random").unwrap_err();
        assert!(matches!(err, ModelError::UnknownRestart(_)));
    }
}
