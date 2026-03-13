use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Logical identifier for a controller slot.
///
/// A slot groups tasks that share a single execution lane.
/// The controller enforces admission policies per slot, ensuring
/// that only one task occupies a slot at a time.
///
/// Internally backed by `Arc<str>` — cloning a `Slot` is an atomic
/// reference-count increment (no heap allocation).
///
/// ```
/// use solti_model::Slot;
///
/// let slot = Slot::new("build-pipeline");
/// assert_eq!(slot.as_str(), "build-pipeline");
///
/// // From &str / String
/// let slot: Slot = "deploy".into();
/// assert_eq!(format!("{slot}"), "deploy");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slot(Arc<str>);

impl Slot {
    /// Create a new slot identifier.
    pub fn new(name: impl Into<String>) -> Self {
        Self(Arc::from(name.into()))
    }

    /// Get the slot name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert into the underlying `Arc<str>`.
    pub fn into_inner(self) -> Arc<str> {
        self.0
    }
}

impl From<String> for Slot {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl From<&str> for Slot {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl AsRef<str> for Slot {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Slot {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Allow `==` comparisons with &str and String for ergonomics.
impl PartialEq<str> for Slot {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Slot {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for Slot {
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

// Manual serde: serialize as plain string, deserialize via String → Arc<str>.
impl Serialize for Slot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Slot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|s| Self(Arc::from(s)))
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;
    use std::sync::Arc;

    #[test]
    fn new_and_as_str() {
        let slot = Slot::new("my-slot");
        assert_eq!(slot.as_str(), "my-slot");
    }

    #[test]
    fn from_str_and_string() {
        let a: Slot = "abc".into();
        let b: Slot = String::from("abc").into();
        assert_eq!(a, b);
    }

    #[test]
    fn display() {
        let slot = Slot::new("demo");
        assert_eq!(format!("{slot}"), "demo");
    }

    #[test]
    fn partial_eq_with_str() {
        let slot = Slot::new("test");
        assert!(slot == *"test");
    }

    #[test]
    fn serde_transparent() {
        let slot = Slot::new("build");
        let json = serde_json::to_string(&slot).unwrap();
        assert_eq!(json, "\"build\"");

        let back: Slot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, slot);
    }

    #[test]
    fn into_inner() {
        let slot = Slot::new("owned");
        let s: Arc<str> = slot.into_inner();
        assert_eq!(&*s, "owned");
    }

    #[test]
    fn clone_is_cheap() {
        let slot = Slot::new("shared");
        let cloned = slot.clone();
        // Both point to the same Arc allocation.
        assert!(Arc::ptr_eq(&slot.0, &cloned.0));
    }
}
