//! Resource identity types.
//!
//! ```text
//!  TaskSpec { slot: "build" }
//!           │
//!           ▼  runner.build()
//!  ┌────────────────────────────────────────────┐
//!  │  Slot: "build"          (stable, logical)  │
//!  │  TaskId: "sub-build-1"  (unique, run)      │
//!  └────────────────────────────────────────────┘
//! ```
//!
//! - [`Slot`]   - logical execution lane, stays the same across submissions.
//! - [`TaskId`] - unique per run, format `{runner}-{slot}-{seq:x}`.
//! - [`AgentId`] - identity of a running agent process.

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

/// Validate that an identifier is safe to use across the SDK.
///
/// **Rules**:
/// - length in `1..=max_len`
/// - characters restricted to `[A-Za-z0-9._-]` _(ASCII, no path separators, no whitespace, no control characters, no Unicode)_
/// - not equal to `"."` or `".."` (path-traversal tokens even without `/`)
///
/// **Why these:**
/// Unvalidated values leak into:
///  - `/sys/fs/cgroup/{name}` directory creation,
///  - `tokio::process::Command` argv
///  - tempfile paths,
///  - logs
///
/// A slot containing `/` creates a nested cgroup dir instead of the intended one;
/// A slot containing `\n` corrupts log parsing;
/// Non-ASCII produces mojibake on containers with unset locale.
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
    fn accepts_simple_alphanumeric() {
        validate_identity("slot", "abc123", 64).unwrap();
        validate_identity("slot", "build.pipeline", 64).unwrap();
        validate_identity("slot", "my_slot-1", 64).unwrap();
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_identity("slot", "", 64).is_err());
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(65);
        assert!(validate_identity("slot", &s, 64).is_err());
    }

    #[test]
    fn rejects_path_separators() {
        assert!(validate_identity("slot", "a/b", 64).is_err());
        assert!(validate_identity("slot", "a\\b", 64).is_err());
    }

    #[test]
    fn rejects_whitespace() {
        assert!(validate_identity("slot", "a b", 64).is_err());
        assert!(validate_identity("slot", "a\tb", 64).is_err());
        assert!(validate_identity("slot", "a\nb", 64).is_err());
    }

    #[test]
    fn rejects_control_chars() {
        assert!(validate_identity("slot", "a\x00b", 64).is_err());
        assert!(validate_identity("slot", "a\x1bb", 64).is_err());
    }

    #[test]
    fn rejects_non_ascii() {
        assert!(validate_identity("slot", "построение", 64).is_err());
        assert!(validate_identity("slot", "a\u{200b}b", 64).is_err());
    }

    #[test]
    fn rejects_dot_dotdot() {
        assert!(validate_identity("slot", ".", 64).is_err());
        assert!(validate_identity("slot", "..", 64).is_err());
        validate_identity("slot", "..x", 64).unwrap();
    }
}
