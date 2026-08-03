//! # Admission policy
//!
//! [`AdmissionPolicy`] describes admission when a slot is busy.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Admission behavior for a busy slot.
///
/// A slot has at most one active owner or admission candidate.
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Reject the new submission while the slot is busy.
    #[default]
    DropIfRunning,
    /// Removes the current owner and places the new submission next.
    Replace,
    /// Append the submission to the slot FIFO queue.
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
    fn parsing_accepts_canonical_names_aliases_case_and_whitespace() {
        let cases = [
            ("", AdmissionPolicy::default()),
            ("drop-if-running", AdmissionPolicy::DropIfRunning),
            ("drop", AdmissionPolicy::DropIfRunning),
            ("DROP", AdmissionPolicy::DropIfRunning),
            ("queue", AdmissionPolicy::Queue),
            ("add", AdmissionPolicy::Queue),
            ("new", AdmissionPolicy::Queue),
            ("  queue  ", AdmissionPolicy::Queue),
            ("replace", AdmissionPolicy::Replace),
            ("REPLACE", AdmissionPolicy::Replace),
        ];
        for (value, expected) in cases {
            assert_eq!(value.parse::<AdmissionPolicy>().unwrap(), expected);
        }
    }

    #[test]
    fn parsing_rejects_unknown_values() {
        assert!("foobar".parse::<AdmissionPolicy>().is_err());
    }
}
