//! # Execution slot
//!
//! [`Slot`] is a logical concurrency key.
//! It accepts `[A-Za-z0-9._-]` and is limited to [`SLOT_MAX_LEN`] bytes.

use super::validate_identity;
use crate::error::ModelError;

/// Maximum length of a `Slot` identifier.
pub const SLOT_MAX_LEN: usize = 64;

arc_str_newtype! {
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::slot"))]
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
    /// Validates the slot.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty, too long, equal to `"."` or `".."`, or contains a byte outside `[A-Za-z0-9._-]`.
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
    fn exposes_string_conversions_and_shares_clones() {
        let slot = Slot::new("shared").unwrap();
        let parsed: Slot = "shared".parse().unwrap();

        assert_eq!(slot.as_str(), "shared");
        assert_eq!(format!("{slot}"), "shared");
        assert_eq!(slot, *"shared");
        assert_eq!(slot, parsed);

        let cloned = slot.clone();
        let a: Arc<str> = slot.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert_eq!(&*a, "shared");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn serde_is_transparent() {
        let slot = Slot::new("build").unwrap();
        let json = serde_json::to_string(&slot).unwrap();
        assert_eq!(json, "\"build\"");
        assert_eq!(serde_json::from_str::<Slot>(&json).unwrap(), slot);
    }

    #[test]
    fn validation_accepts_safe_values_and_rejects_unsafe_values() {
        for valid in ["build.frontend", "build", "a"] {
            Slot::new(valid).unwrap();
        }
        for invalid in ["build/frontend", "é", "with space", "a\nb", ".", ""] {
            assert!(Slot::new(invalid).is_err(), "must reject {invalid:?}");
        }
    }
}
