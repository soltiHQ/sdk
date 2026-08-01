//! Shared runner registration rules.

/// Runner label used by `runnerSelector`.
pub const LABEL_RUNNER_NAME: &str = "solti.io/runner-name";

/// Validates a runner name before it is used in labels and runtime identifiers.
pub(crate) fn validate_runner_name(name: &str) -> Result<(), crate::ExecError> {
    let edge_is_alphanumeric = name
        .as_bytes()
        .first()
        .zip(name.as_bytes().last())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    let valid = name.len() <= 63
        && edge_is_alphanumeric
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));

    if valid {
        Ok(())
    } else {
        Err(crate::ExecError::InvalidRunnerConfig(format!(
            "invalid runner name {name:?}: must be a Kubernetes label value of 1..=63 ASCII characters"
        )))
    }
}
