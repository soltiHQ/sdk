//! Write preconditions.
//!
//! [`WritePreconditions`] protects an apply or delete from a stale resource snapshot.

use crate::{ModelError, ModelResult, Task, Uid};

/// Optional identity and version checks for a resource write.
///
/// When both values are present, both must match the current resource.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WritePreconditions {
    uid: Option<Uid>,
    resource_version: Option<String>,
}

impl WritePreconditions {
    /// Create empty preconditions.
    #[inline]
    pub const fn new() -> Self {
        Self {
            uid: None,
            resource_version: None,
        }
    }

    /// Capture the current resource identity and version.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for a resource not yet stored.
    pub fn from_task(task: &Task) -> ModelResult<Self> {
        Self::new()
            .with_uid(task.uid().clone())
            .with_resource_version(task.metadata().resource_version())
    }

    /// Require the current resource incarnation to have this UID.
    #[inline]
    pub fn with_uid(mut self, uid: Uid) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Require the current resource to have this resource version.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty.
    pub fn with_resource_version(
        mut self,
        resource_version: impl Into<String>,
    ) -> ModelResult<Self> {
        let resource_version = resource_version.into();
        if resource_version.trim().is_empty() {
            return Err(ModelError::Invalid(
                "resourceVersion precondition must not be empty".into(),
            ));
        }
        self.resource_version = Some(resource_version);
        Ok(self)
    }

    /// Expected resource UID, when present.
    #[inline]
    pub fn uid(&self) -> Option<&Uid> {
        self.uid.as_ref()
    }

    /// Expected resource version, when present.
    #[inline]
    pub fn resource_version(&self) -> Option<&str> {
        self.resource_version.as_deref()
    }

    /// Return whether no checks were requested.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.uid.is_none() && self.resource_version.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_unconditional() {
        assert!(WritePreconditions::new().is_empty());
    }

    #[test]
    fn values_are_retained() {
        let uid = Uid::new("resource-uid").unwrap();
        let preconditions = WritePreconditions::new()
            .with_uid(uid.clone())
            .with_resource_version("42")
            .unwrap();

        assert_eq!(preconditions.uid(), Some(&uid));
        assert_eq!(preconditions.resource_version(), Some("42"));
        assert!(!preconditions.is_empty());
    }

    #[test]
    fn empty_resource_version_is_rejected() {
        let error = WritePreconditions::new()
            .with_resource_version(" ")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid model: resourceVersion precondition must not be empty"
        );
    }

    #[test]
    fn unstored_task_cannot_be_captured() {
        let task = Task::from_manifest(
            crate::TaskManifest::new(
                "unstored",
                crate::TaskSpec::builder(
                    "slot",
                    crate::TaskWorkload::Embedded(crate::EmbeddedSpec::new("test").unwrap()),
                    1_000_u64,
                )
                .build()
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();

        assert!(WritePreconditions::from_task(&task).is_err());
    }
}
