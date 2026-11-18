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

/// Decodes the body of the workload a runner is asked to build.
///
/// A workload of another type, or an embedded one, is [`solti_runner::RunnerError::UnsupportedWorkload`].
/// A body of this runner's type that does not decode or validate is [`solti_runner::RunnerError::InvalidSpec`].
pub(crate) fn decode_workload<B: solti_model::WorkloadBody>(
    runner: &str,
    workload: &solti_model::TaskWorkload,
) -> Result<B, solti_runner::RunnerError> {
    use solti_model::{TaskWorkload, WorkloadBodyErrorKind};
    use solti_runner::RunnerError;

    let unsupported = |workload: &TaskWorkload| RunnerError::UnsupportedWorkload {
        runner: runner.to_owned(),
        api_version: workload.api_version().to_owned(),
        kind: workload.kind().to_owned(),
    };
    match workload {
        TaskWorkload::Routed(routed) => routed.decode::<B>().map_err(|error| match error.kind() {
            WorkloadBodyErrorKind::TypeMismatch { .. } => unsupported(workload),
            _ => RunnerError::InvalidSpec(error.to_string()),
        }),
        other => Err(unsupported(other)),
    }
}
