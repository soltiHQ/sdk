//! # Container workload spec
//!
//! [`ContainerSpec`] is the body of the `exec.solti.io/v1` `Container` workload.

use serde::{Deserialize, Serialize};
use solti_model::{ModelError, ModelResult, TaskEnv};

/// Specification for OCI-compatible container execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// Container image (e.g. `"nginx:latest"`, `"docker.io/library/redis:7"`).
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::non_empty_string")
    )]
    pub image: String,
    /// Override container entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Arguments passed to the container entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the container.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
}

impl ContainerSpec {
    /// Creates a container spec.
    ///
    /// `ContainerSpec` is `#[non_exhaustive]`.
    /// Use this constructor outside the crate.
    /// Validation occurs when the workload enters a [`crate::TaskSpec`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv};
    ///
    /// let spec = ContainerSpec::new(
    ///     "docker.io/library/redis:7".into(),
    ///     None,
    ///     vec![],
    ///     TaskEnv::default(),
    /// );
    ///
    /// assert_eq!(spec.image, "docker.io/library/redis:7");
    /// ```
    pub fn new(
        image: String,
        command: Option<Vec<String>>,
        args: Vec<String>,
        env: TaskEnv,
    ) -> Self {
        Self {
            image,
            command,
            args,
            env,
        }
    }

    /// Validates structural constraints.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the image is empty.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ContainerSpec, TaskEnv};
    ///
    /// let spec = ContainerSpec::new("redis:7".into(), None, vec![], TaskEnv::default());
    /// spec.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        if self.image.trim().is_empty() {
            return Err(ModelError::Invalid(
                "container image cannot be empty".into(),
            ));
        }
        Ok(())
    }
}
