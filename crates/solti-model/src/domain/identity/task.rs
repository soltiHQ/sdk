//! # Task identity
//!
//! [`TaskId`] is the stable name of a task resource.
//! It follows the Kubernetes DNS-1123 subdomain format.

use crate::error::ModelError;

/// Maximum length of a `TaskId`.
pub const TASK_ID_MAX_LEN: usize = crate::validation::DNS1123_SUBDOMAIN_MAX_LEN;

arc_str_newtype! {
    /// Stable name used to address a task resource.
    ///
    /// Apply, get and delete operations address a task through this name.
    /// [`Uid`](crate::Uid) separately identifies a particular incarnation when a name is deleted and recreated.
    ///
    /// ```
    /// use solti_model::TaskId;
    ///
    /// let id = TaskId::new("subprocess-build-1").unwrap();
    /// assert_eq!(id.as_str(), "subprocess-build-1");
    /// ```
    pub struct TaskId;
}

impl TaskId {
    /// Validates the task id.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskId;
    ///
    /// assert!(TaskId::new("subprocess-build-1").is_ok());
    /// assert!(TaskId::new("with/slash").is_err());
    /// ```
    pub fn validate_format(&self) -> Result<(), ModelError> {
        crate::validation::validate_dns1123_subdomain("task_id", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn exposes_string_identity_hashing_and_shared_clones() {
        use std::collections::HashSet;

        let id = TaskId::new("id-1").unwrap();
        assert_eq!(id.as_str(), "id-1");
        assert_eq!(format!("{id}"), "id-1");

        let mut set = HashSet::new();
        set.insert(id.clone());
        set.insert(TaskId::new("id-2").unwrap());
        set.insert(TaskId::new("id-1").unwrap());
        assert_eq!(set.len(), 2);

        let cloned = id.clone();
        let a: Arc<str> = id.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn serde_is_transparent_and_validated() {
        let id = TaskId::new("runner-slot-ff").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""runner-slot-ff""#);
        assert_eq!(serde_json::from_str::<TaskId>(&json).unwrap(), id);

        for invalid in [
            r#""a/b""#,
            r#""""#,
            r#"".""#,
            r#""a b""#,
            r#""UPPER""#,
            r#""under_score""#,
        ] {
            assert!(
                serde_json::from_str::<TaskId>(invalid).is_err(),
                "must reject {invalid}"
            );
        }
    }

    #[test]
    fn validation_matches_kubernetes_resource_names_and_length_limit() {
        for valid in ["subprocess-build-1", "subprocess-build.frontend-ff"] {
            TaskId::new(valid).unwrap();
        }
        for invalid in [
            "",
            "with/slash",
            "with space",
            "UPPER",
            "with_underscore",
            "-leading",
            "trailing-",
            "empty..label",
        ] {
            assert!(TaskId::new(invalid).is_err(), "must reject {invalid:?}");
        }
        assert!(TaskId::new("x".repeat(64)).is_ok());
        let max = "a".repeat(TASK_ID_MAX_LEN);
        assert!(TaskId::new(&max).is_ok());
        assert!(TaskId::new(format!("{max}e")).is_err());
    }
}
