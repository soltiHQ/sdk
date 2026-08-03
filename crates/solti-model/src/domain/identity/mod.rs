//! # Identities
//!
//! | Type        | Meaning                    | Validation                    |
//! |-------------|----------------------------|-------------------------------|
//! | [`TaskId`]  | Stable task resource name  | DNS-1123 subdomain            |
//! | [`Slot`]    | Logical concurrency key    | `[A-Za-z0-9._-]`, 64 bytes    |
//! | [`AgentId`] | Agent identity             | `[A-Za-z0-9._-]`, 128 bytes   |

#[macro_use]
mod macros;

mod agent;
pub use agent::{AGENT_ID_MAX_LEN, AgentId};

mod slot;
pub use slot::{SLOT_MAX_LEN, Slot};

mod task;
pub use task::{TASK_ID_MAX_LEN, TaskId};

use crate::error::ModelError;
use std::borrow::Cow;

/// Validates a shared SDK identifier.
///
/// Values use `[A-Za-z0-9._-]`.
/// `"."` and `".."` are rejected.
pub(crate) fn validate_identity(kind: &'static str, s: &str, l: usize) -> Result<(), ModelError> {
    if s.is_empty() {
        return Err(ModelError::Invalid(Cow::Owned(format!(
            "{kind} must not be empty"
        ))));
    }
    if s.len() > l {
        return Err(ModelError::Invalid(Cow::Owned(format!(
            "{kind} length {} exceeds max {l}",
            s.len()
        ))));
    }
    if s == "." || s == ".." {
        return Err(ModelError::Invalid(Cow::Owned(format!(
            "{kind} cannot be '.' or '..'"
        ))));
    }
    for (i, ch) in s.bytes().enumerate() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'_' | b'-');
        if !ok {
            return Err(ModelError::Invalid(Cow::Owned(format!(
                "{kind} contains forbidden byte 0x{ch:02x} at position {i} \
                 (allowed: [A-Za-z0-9._-])"
            ))));
        }
    }
    Ok(())
}

#[cfg(test)]
mod validate_tests {
    use super::validate_identity;

    #[test]
    fn accepts_safe_ascii_values() {
        for value in ["abc123", "build.pipeline", "my_slot-1", "..x"] {
            validate_identity("slot", value, 64).unwrap();
        }
    }

    #[test]
    fn rejects_unsafe_or_oversized_values() {
        for value in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a b",
            "a\tb",
            "a\nb",
            "a\x00b",
            "a\x1bb",
            "é",
            "a\u{200b}b",
        ] {
            assert!(
                validate_identity("slot", value, 64).is_err(),
                "must reject {value:?}"
            );
        }
        assert!(validate_identity("slot", &"a".repeat(65), 64).is_err());
    }
}
