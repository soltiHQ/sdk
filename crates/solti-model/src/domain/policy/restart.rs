//! # Restart policy
//!
//! [`RestartPolicy`] describes whether another attempt may be started.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Policy for starting another attempt.
///
/// | Variant     | Meaning                              |
/// |-------------|--------------------------------------|
/// | `Never`     | Do not start another attempt         |
/// | `OnFailure` | Restart after a retryable failure    |
/// | `Always`    | Restart after any completed attempt  |
///
/// `Always { interval_ms: None }` requests an immediate restart.
/// `Always { interval_ms: Some(n) }` requests a delay of `n` milliseconds.
///
/// ## Example
///
/// ```
/// use solti_model::RestartPolicy;
///
/// let retry_errors = RestartPolicy::OnFailure;
/// let service = RestartPolicy::always();
/// let periodic = RestartPolicy::periodic(60_000);
///
/// let _ = (retry_errors, service, periodic);
/// ```
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Never restart the task.
    #[default]
    Never,
    /// Restart after a retryable failure.
    OnFailure,
    /// Restart after every completed attempt.
    #[serde(rename_all = "camelCase")]
    Always {
        /// Delay between attempts in milliseconds.
        ///
        /// `None` and `Some(0)` request an immediate restart.
        #[serde(skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u64>,
    },
}

impl RestartPolicy {
    /// Creates an immediate `Always` policy.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::RestartPolicy;
    ///
    /// assert_eq!(
    ///     RestartPolicy::always(),
    ///     RestartPolicy::Always { interval_ms: None },
    /// );
    /// ```
    pub const fn always() -> Self {
        RestartPolicy::Always { interval_ms: None }
    }

    /// Creates a periodic `Always` policy.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::RestartPolicy;
    ///
    /// assert_eq!(
    ///     RestartPolicy::periodic(5_000),
    ///     RestartPolicy::Always { interval_ms: Some(5_000) },
    /// );
    /// ```
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
    fn parsing_accepts_policies_aliases_case_and_intervals() {
        let cases = [
            ("", RestartPolicy::Never),
            ("never", RestartPolicy::Never),
            ("  NeVeR  ", RestartPolicy::Never),
            ("on-failure", RestartPolicy::OnFailure),
            ("failure", RestartPolicy::OnFailure),
            ("  Failure ", RestartPolicy::OnFailure),
            ("always", RestartPolicy::Always { interval_ms: None }),
            ("  ALWAYS  ", RestartPolicy::Always { interval_ms: None }),
            ("always:", RestartPolicy::Always { interval_ms: None }),
            ("always:   ", RestartPolicy::Always { interval_ms: None }),
            (
                "always:1000",
                RestartPolicy::Always {
                    interval_ms: Some(1_000),
                },
            ),
            (
                " Always:  60000 ",
                RestartPolicy::Always {
                    interval_ms: Some(60_000),
                },
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(RestartPolicy::from_str(value).unwrap(), expected);
        }
    }

    #[test]
    fn parsing_rejects_unknown_policy_and_invalid_interval() {
        for value in ["always:not-a-number", "random"] {
            assert!(matches!(
                RestartPolicy::from_str(value),
                Err(ModelError::UnknownRestart(_))
            ));
        }
    }
}
