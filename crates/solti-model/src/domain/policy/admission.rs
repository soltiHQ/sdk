//! Admission policy.
//!
//! [`AdmissionPolicy`] controls how a new submission targets a busy slot.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Defines how the controller admits a new task into a slot.
///
/// A slot tracks at most one active owner or admission candidate. It remains busy
/// during admission, while a registered owner is alive (including backoff or time
/// between attempts), and while that owner is being removed. When a new submission
/// arrives, this policy says what to do with it.
///
/// | Variant         | Behaviour                                                        |
/// |-----------------|------------------------------------------------------------------|
/// | `DropIfRunning` | Reject the new submission while the slot is busy                 |
/// | `Replace`       | Request owner removal and make the new submission next           |
/// | `Queue`         | Append to the bounded FIFO queue and admit when the slot is free  |
///
/// ## Example
///
/// ```
/// use solti_model::AdmissionPolicy;
///
/// assert_eq!("replace".parse::<AdmissionPolicy>().unwrap(), AdmissionPolicy::Replace);
/// assert_eq!("queue".parse::<AdmissionPolicy>().unwrap(), AdmissionPolicy::Queue);
/// ```
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Reject the new submission if the slot is busy in any lifecycle phase.
    #[default]
    DropIfRunning,
    /// Request removal of the current owner and put the new submission next.
    /// A later replacement supersedes a replacement that is still pending.
    Replace,
    /// Append the submission to the bounded FIFO queue for this slot.
    Queue,
}

impl FromStr for AdmissionPolicy {
    type Err = ModelError;
    fn from_str(s: &str) -> ModelResult<Self> {
        let s = s.trim();
        if s.is_empty()
            || s.eq_ignore_ascii_case("drop-if-running")
            || s.eq_ignore_ascii_case("drop")
        {
            Ok(AdmissionPolicy::DropIfRunning)
        } else if s.eq_ignore_ascii_case("queue")
            || s.eq_ignore_ascii_case("add")
            || s.eq_ignore_ascii_case("new")
        {
            Ok(AdmissionPolicy::Queue)
        } else if s.eq_ignore_ascii_case("replace") {
            Ok(AdmissionPolicy::Replace)
        } else {
            Err(ModelError::UnknownAdmission(s.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drop_if_running_variants() {
        assert_eq!(
            "drop-if-running".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::DropIfRunning
        );
        assert_eq!(
            "drop".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::DropIfRunning
        );
        assert_eq!(
            "DROP".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::DropIfRunning
        );
    }

    #[test]
    fn parse_queue_variants() {
        assert_eq!(
            "queue".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Queue
        );
        assert_eq!(
            "add".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Queue
        );
        assert_eq!(
            "new".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Queue
        );
    }

    #[test]
    fn parse_replace() {
        assert_eq!(
            "replace".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Replace
        );
        assert_eq!(
            "REPLACE".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Replace
        );
    }

    #[test]
    fn empty_string_maps_to_default() {
        let parsed: AdmissionPolicy = "".parse().unwrap();
        assert_eq!(parsed, AdmissionPolicy::default());
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            "  queue  ".parse::<AdmissionPolicy>().unwrap(),
            AdmissionPolicy::Queue
        );
    }

    #[test]
    fn unknown_value_fails() {
        assert!("foobar".parse::<AdmissionPolicy>().is_err());
    }
}
