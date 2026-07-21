//! Task identifier.
//!
//! [`TaskId`] is the stable name of a task resource (newtype over `Arc<str>`).

use crate::error::ModelError;

/// Maximum length of a `TaskId`.
pub const TASK_ID_MAX_LEN: usize = crate::validation::DNS1123_SUBDOMAIN_MAX_LEN;

arc_str_newtype! {
    /// Stable name used to address a task resource.
    ///
    /// Apply, get and delete operations address a task through this name.
    /// [`Uid`](crate::Uid) separately identifies a particular incarnation when a
    /// name is deleted and recreated.
    ///
    /// ```
    /// use solti_model::TaskId;
    ///
    /// let id = TaskId::new("subprocess-build-1");
    /// assert_eq!(id.as_str(), "subprocess-build-1");
    /// ```
    pub struct TaskId;
}

impl TaskId {
    /// Validate that the task id is safe to use across the SDK.
    ///
    /// Uses the Kubernetes DNS-1123 subdomain rules for `metadata.name`.
    ///
    /// ## Errors
    ///
    /// - [`ModelError::Invalid`]: the id is empty, longer than [`TASK_ID_MAX_LEN`],
    ///   has an empty dot-separated segment, contains a byte outside
    ///   `[a-z0-9.-]`, or a label does not start and end with an alphanumeric byte.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskId;
    ///
    /// assert!(TaskId::new("subprocess-build-1").validate_format().is_ok());
    /// assert!(TaskId::new("with/slash").validate_format().is_err());
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
    fn task_id_from_string() {
        let id = TaskId::from("subprocess-slot-2a");
        assert_eq!(id.as_str(), "subprocess-slot-2a");
    }

    #[test]
    fn task_id_display() {
        let id = TaskId::new("test-id");
        assert_eq!(format!("{}", id), "test-id");
    }

    #[test]
    fn task_id_serde_transparent() {
        let id = TaskId::from("runner-slot-ff");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""runner-slot-ff""#);

        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn task_id_deserialization_validates_format() {
        for bad in [
            r#""a/b""#,
            r#""""#,
            r#"".""#,
            r#""a b""#,
            r#""UPPER""#,
            r#""under_score""#,
        ] {
            assert!(
                serde_json::from_str::<TaskId>(bad).is_err(),
                "must reject {bad}"
            );
        }
    }

    #[test]
    fn task_id_hash_equality() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(TaskId::from("id-1"));
        set.insert(TaskId::from("id-2"));
        set.insert(TaskId::from("id-1"));

        assert_eq!(set.len(), 2);
        assert!(set.contains(&TaskId::from("id-1")));
    }

    #[test]
    fn clone_is_cheap() {
        let id = TaskId::new("shared-task");
        let cloned = id.clone();
        let a: Arc<str> = id.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn validate_format_accepts_runner_generated() {
        TaskId::new("subprocess-build-1").validate_format().unwrap();
        TaskId::new("subprocess-build.frontend-ff")
            .validate_format()
            .unwrap();
    }

    #[test]
    fn validate_format_rejects_invalid() {
        assert!(TaskId::new("").validate_format().is_err());
        assert!(TaskId::new("with/slash").validate_format().is_err());
        assert!(TaskId::new("with space").validate_format().is_err());
        assert!(TaskId::new("UPPER").validate_format().is_err());
        assert!(TaskId::new("with_underscore").validate_format().is_err());
        assert!(TaskId::new("-leading").validate_format().is_err());
        assert!(TaskId::new("trailing-").validate_format().is_err());
        assert!(TaskId::new("empty..label").validate_format().is_err());
        assert!(TaskId::new(&"x".repeat(64)).validate_format().is_ok());
        let max = "a".repeat(TASK_ID_MAX_LEN);
        assert_eq!(max.len(), TASK_ID_MAX_LEN);
        assert!(TaskId::new(&max).validate_format().is_ok());
        let too_long = format!("{max}e");
        assert!(TaskId::new(&too_long).validate_format().is_err());
    }
}
