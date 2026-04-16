//! # Execution slot.
//!
//! [`Slot`] is the logical execution lane name (newtype over `Arc<str>`).

use super::macros::arc_str_newtype;

arc_str_newtype! {
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
    pub struct Slot;
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
        let a: Arc<str> = slot.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
