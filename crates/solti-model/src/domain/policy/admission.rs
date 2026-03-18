use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Defines how the controller admits a new task into a slot.
///
/// A slot may only run one task at a time.
/// When a new task arrives, the controller applies the selected policy to determine what to do if the slot is already occupied.
///
/// | Variant         | Behaviour                                              |
/// |-----------------|--------------------------------------------------------|
/// | `DropIfRunning` | Ignore the new task, return success without scheduling |
/// | `Replace`       | Cancel the running task, schedule the new one          |
/// | `Queue`         | Enqueue the new task, run when slot becomes free       |
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// If the slot already has a running task, ignore the new one.
    #[default]
    DropIfRunning,
    /// Cancel the currently running task in the slot and replace it with the newly submitted task.
    Replace,
    /// Enqueue the new task to be executed after the current one completes.
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
