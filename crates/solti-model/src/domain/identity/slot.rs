//! Execution slot.
//!
//! [`Slot`] is the logical execution lane name (newtype over `Arc<str>`).

use super::validate_identity;
use crate::error::ModelError;

/// Maximum length of a `Slot` identifier.
pub const SLOT_MAX_LEN: usize = 64;

arc_str_newtype! {
    /// Logical identifier for a controller slot.
    ///
    /// A slot groups tasks that share a single execution lane.
    /// Controllers use slots for admission policy and queue behavior.
    ///
    /// ```rust
    /// use solti_model::Slot;
    ///
    /// let slot = Slot::new("build-pipeline").unwrap();
    /// assert_eq!(slot.as_str(), "build-pipeline");
    ///
    /// let slot = Slot::new("deploy").unwrap();
    /// assert_eq!(format!("{slot}"), "deploy");
    /// ```
    pub struct Slot;
}

impl Slot {
    /// Validate that the slot name is safe to use across the SDK.
    ///
    /// See `validate_identity` (module-private) for the exact rules.
    ///
    /// ## Errors
    ///
    /// - [`ModelError::Invalid`]: the name is empty, longer than [`SLOT_MAX_LEN`],
    ///   equal to `"."` or `".."`, or contains a byte outside `[A-Za-z0-9._-]`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Slot;
    ///
    /// assert!(Slot::new("build.frontend").is_ok());
    /// assert!(Slot::new("build/frontend").is_err());
    /// ```
    pub fn validate_format(&self) -> Result<(), ModelError> {
        validate_identity("slot", self.as_str(), SLOT_MAX_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;
    use std::sync::Arc;

    #[test]
    fn new_and_as_str() {
        let slot = Slot::new("my-slot").unwrap();
        assert_eq!(slot.as_str(), "my-slot");
    }

    #[test]
    fn from_str_and_string() {
        let a = Slot::new("abc").unwrap();
        let b: Slot = "abc".parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn display() {
        let slot = Slot::new("demo").unwrap();
        assert_eq!(format!("{slot}"), "demo");
    }

    #[test]
    fn partial_eq_with_str() {
        let slot = Slot::new("test").unwrap();
        assert_eq!(slot, *"test");
    }

    #[test]
    fn serde_transparent() {
        let slot = Slot::new("build").unwrap();
        let json = serde_json::to_string(&slot).unwrap();
        assert_eq!(json, "\"build\"");

        let back: Slot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, slot);
    }

    #[test]
    fn into_inner() {
        let slot = Slot::new("owned").unwrap();
        let s: Arc<str> = slot.into_inner();
        assert_eq!(&*s, "owned");
    }

    #[test]
    fn clone_is_cheap() {
        let slot = Slot::new("shared").unwrap();
        let cloned = slot.clone();
        let a: Arc<str> = slot.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn validate_format_accepts_valid() {
        Slot::new("build.frontend").unwrap();
        Slot::new("build").unwrap();
        Slot::new("a").unwrap();
    }

    #[test]
    fn validate_format_rejects_invalid() {
        assert!(Slot::new("build/frontend").is_err());
        let non_ascii = String::from_utf8(vec![0xc3, 0xa9]).unwrap();
        assert!(Slot::new(&non_ascii).is_err());
        assert!(Slot::new("with space").is_err());
        assert!(Slot::new("a\nb").is_err());
        assert!(Slot::new(".").is_err());
        assert!(Slot::new("").is_err());
    }
}
