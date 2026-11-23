//! # Container workload spec
//!
//! [`ContainerSpec`] is the body of the `exec.solti.io/v1` `Container` workload.

use serde::{Deserialize, Serialize};
use solti_model::{TaskEnv, WorkloadBody};

use crate::EXEC_API_VERSION;
use crate::registration::at_field as at;

/// Kind of the container workload under [`EXEC_API_VERSION`].
pub const CONTAINER_KIND: &str = "Container";

/// Specification for OCI-compatible container execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// Container image (e.g. `"nginx:latest"`, `"docker.io/library/redis:7"`).
    #[schemars(schema_with = "crate::schema::non_empty_string")]
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
    /// Validation occurs when the runner decodes the workload body.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_exec::container::ContainerSpec;
    /// use solti_model::TaskEnv;
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
}

impl WorkloadBody for ContainerSpec {
    const API_VERSION: &'static str = EXEC_API_VERSION;
    const KIND: &'static str = CONTAINER_KIND;

    /// Requires a non-empty image, and process input the runner accepts.
    ///
    /// The command, the arguments and the environment values must hold no NUL byte, and
    /// each environment name must be non-empty and hold no `=` or NUL: the rules the runner
    /// applies when it builds the task, so admission refuses what the build would. These
    /// refusals name the field, as `spec.env[0].key: …`.
    fn validate(&self) -> Result<(), String> {
        if self.image.trim().is_empty() {
            return Err("container image cannot be empty".into());
        }
        for (index, word) in self.command.iter().flatten().enumerate() {
            at(
                format_args!("command[{index}]"),
                validate_process_word(word),
            )?;
        }
        for (index, arg) in self.args.iter().enumerate() {
            at(format_args!("args[{index}]"), validate_process_word(arg))?;
        }
        for (index, entry) in self.env.iter().enumerate() {
            at(
                format_args!("env[{index}].key"),
                validate_env_name(entry.key()),
            )?;
            at(
                format_args!("env[{index}].value"),
                validate_env_value(entry.key(), entry.value()),
            )?;
        }
        Ok(())
    }
}

/// Refuses a command word or an argument with a NUL byte.
pub(super) fn validate_process_word(value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err("container command and arguments cannot contain NUL".into());
    }
    Ok(())
}

/// Refuses an empty environment variable name, or one with `=` or NUL.
pub(super) fn validate_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(format!(
            "invalid container environment variable name {name:?}"
        ));
    }
    Ok(())
}

/// Refuses a NUL byte in the value of the environment variable `name`.
pub(super) fn validate_env_value(name: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!(
            "container environment variable {name:?} contains NUL"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {}
