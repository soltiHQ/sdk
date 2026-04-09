//! # Execution slot.
//!
//! [`Slot`] is the logical execution lane name (newtype over `Arc<str>`).

use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Logical identifier for a controller slot.
///
/// A slot groups tasks that share a single execution lane.
///
/// ```text
///  Slot: "build-pipeline"         Slot: "deploy"
///  ┌───────────────────────┐      ┌────────────────────────┐
///  │  TaskId: sub-build-1  │      │  TaskId: sub-deploy-1  │
///  │  TaskId: sub-build-2  │      │  TaskId: sub-deploy-2  │
///  │  TaskId: sub-build-3  │      │  TaskId: sub-deploy-3  │
///  │        ...            │      │        ...             │
///  └───────────────────────┘      └────────────────────────┘
///         one lane                        one lane
///     (one at a time)                  (one at a time)
/// ```
/// 
/// ```rust
/// use solti_model::Slot;
///
/// let slot = Slot::new("build-pipeline");
/// assert_eq!(slot.as_str(), "build-pipeline");
///
/// let slot: Slot = "deploy".into();
/// assert_eq!(format!("{slot}"), "deploy");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(Arc<str>);

impl Slot {
    /// Create a new slot identifier.
    #[inline]
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    /// Get the slot name as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert into the underlying `Arc<str>`.
    #[inline]
    pub fn into_inner(self) -> Arc<str> {
        self.0
    }
}

impl From<String> for Slot {
    #[inline]
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl From<&str> for Slot {
    #[inline]
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<Arc<str>> for Slot {
    #[inline]
    fn from(s: Arc<str>) -> Self {
        Self(s)
    }
}

impl AsRef<str> for Slot {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Slot {
    #[inline]
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Slot {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Slot {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Slot {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for Slot {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

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
        assert_eq!(slot, *"test");
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
        assert!(Arc::ptr_eq(&slot.0, &cloned.0));
    }
}
