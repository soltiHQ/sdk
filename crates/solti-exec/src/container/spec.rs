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
mod tests {
    use super::*;
    use crate::EXEC_API_VERSION;
    use solti_model::{Workload, WorkloadBodyErrorKind, workload_schema_for};

    fn routed_body(body: serde_json::Value) -> Workload {
        Workload::new(EXEC_API_VERSION, CONTAINER_KIND, body).unwrap()
    }

    #[test]
    fn spec_validation_accepts_an_image_and_rejects_an_empty_one() {
        let spec = ContainerSpec::new("nginx:latest".into(), None, vec![], TaskEnv::default());
        WorkloadBody::validate(&spec).unwrap();

        for image in ["", "  \t"] {
            let spec = ContainerSpec::new(image.into(), None, vec![], TaskEnv::default());
            let error = WorkloadBody::validate(&spec).unwrap_err();
            assert!(error.contains("container image"), "{error}");

            let error = routed_body(serde_json::json!({"image": image}))
                .decode::<ContainerSpec>()
                .unwrap_err();
            assert_eq!(error.kind(), &WorkloadBodyErrorKind::Invalid);
            assert!(error.message().contains("container image"), "{error}");
        }
    }

    #[test]
    fn constructor_builds_a_spec_with_the_expected_fields() {
        let spec = ContainerSpec::new(
            "img:1".into(),
            Some(vec!["sh".into()]),
            vec!["-c".into()],
            TaskEnv::default(),
        );
        assert_eq!(spec.image, "img:1");
        assert_eq!(spec.command, Some(vec!["sh".to_string()]));
        assert_eq!(spec.args, vec!["-c".to_string()]);
    }

    #[test]
    fn body_declares_exec_container() {
        assert_eq!(ContainerSpec::API_VERSION, "exec.solti.io/v1");
        assert_eq!(ContainerSpec::KIND, "Container");
        assert_eq!(CONTAINER_KIND, "Container");
    }

    #[test]
    fn container_body_json_shape_is_unchanged() {
        let body = serde_json::json!({
            "image": "redis:7",
            "command": ["sh"],
            "args": ["-c", "true"],
            "env": [{"key": "A", "value": "1"}],
        });
        let decoded = routed_body(body.clone()).decode::<ContainerSpec>().unwrap();
        assert_eq!(decoded.image, "redis:7");
        assert_eq!(decoded.command, Some(vec!["sh".to_owned()]));
        assert_eq!(decoded.args, vec!["-c".to_owned(), "true".to_owned()]);
        assert_eq!(decoded.env, TaskEnv::single("A", "1"));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), body);
        assert_eq!(Workload::from_body(&decoded).unwrap().body(), &body);

        let minimal = routed_body(serde_json::json!({"image": "redis:7"}))
            .decode::<ContainerSpec>()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            serde_json::json!({"image": "redis:7"})
        );
    }

    #[test]
    fn container_body_refuses_unknown_fields() {
        let error = routed_body(serde_json::json!({"image": "x", "extra": 1}))
            .decode::<ContainerSpec>()
            .unwrap_err();
        assert!(
            matches!(error.kind(), WorkloadBodyErrorKind::Decode { .. }),
            "{error}"
        );
    }

    #[test]
    fn builtin_body_schema_has_no_unbounded_64_bit_integers() {
        let schema = workload_schema_for::<ContainerSpec>();
        assert_eq!(
            crate::schema::testing::unbounded_integers(&schema),
            Vec::<String>::new()
        );
    }
}
