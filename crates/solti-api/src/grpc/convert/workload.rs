//! # Workload Conversion
//!
//! Converts built-in and extension workloads in both directions.
//! Embedded workloads have no public protobuf representation.

use std::path::Path;

use solti_model::{
    ContainerSpec, ExtensionWorkload, Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskWorkload,
    WORKLOAD_API_VERSION, WasmSpec,
};

use crate::{error::ApiError, proto_api};

pub(super) fn workload_to_proto(
    workload: &TaskWorkload,
) -> Result<proto_api::TaskWorkload, ApiError> {
    let spec = match workload {
        TaskWorkload::Subprocess(subprocess) => {
            let mode = match &subprocess.mode {
                SubprocessMode::Command { command, args } => {
                    proto_api::subprocess_mode::Mode::Command(proto_api::CommandMode {
                        command: command.clone(),
                        args: args.clone(),
                    })
                }
                SubprocessMode::Script {
                    interpreter,
                    body,
                    args,
                } => proto_api::subprocess_mode::Mode::Script(proto_api::ScriptMode {
                    interpreter: interpreter.clone(),
                    body: body.clone(),
                    args: args.clone(),
                }),
                _ => {
                    return Err(ApiError::Internal(
                        "unsupported subprocess mode variant".into(),
                    ));
                }
            };
            proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                mode: Some(proto_api::SubprocessMode { mode: Some(mode) }),
                env: env_to_proto(&subprocess.env),
                cwd: subprocess
                    .cwd
                    .as_deref()
                    .map(|path| wire_path_to_proto("subprocess cwd", path))
                    .transpose()?,
                fail_on_non_zero: subprocess.fail_on_non_zero.into(),
            })
        }
        TaskWorkload::Wasm(wasm) => proto_api::task_workload::Spec::Wasm(proto_api::WasmTask {
            module: wire_path_to_proto("wasm module path", &wasm.module)?,
            env: env_to_proto(&wasm.env),
            args: wasm.args.clone(),
        }),
        TaskWorkload::Container(container) => {
            proto_api::task_workload::Spec::Container(proto_api::ContainerTask {
                command: container
                    .command
                    .as_ref()
                    .map(|items| proto_api::ContainerCommand {
                        items: items.clone(),
                    }),
                env: env_to_proto(&container.env),
                image: container.image.clone(),
                args: container.args.clone(),
            })
        }
        TaskWorkload::Embedded(_) => {
            return Err(ApiError::Internal(
                "handler returned an Embedded workload with no wire representation".into(),
            ));
        }
        TaskWorkload::Extension(extension) => {
            proto_api::task_workload::Spec::Extension(proto_api::ExtensionTask {
                spec: Some(proto_api::RawExtension {
                    raw: serde_json::to_vec(extension.spec()).map_err(|error| {
                        ApiError::Internal(format!(
                            "failed to encode extension workload spec: {error}"
                        ))
                    })?,
                }),
            })
        }
        _ => {
            return Err(ApiError::Internal(
                "handler returned a workload kind with no public wire representation".into(),
            ));
        }
    };
    Ok(proto_api::TaskWorkload {
        api_version: workload.api_version().to_owned(),
        kind: workload.kind().to_owned(),
        spec: Some(spec),
    })
}

pub(super) fn convert_task_workload(
    workload: proto_api::TaskWorkload,
) -> Result<TaskWorkload, ApiError> {
    let api_version = workload.api_version;
    let kind = workload.kind;
    let spec = workload
        .spec
        .ok_or_else(|| ApiError::InvalidRequest("missing workload spec".into()))?;

    match spec {
        proto_api::task_workload::Spec::Subprocess(subprocess) => {
            validate_builtin_workload_gvk(&api_version, &kind, "Subprocess")?;
            let mode = subprocess
                .mode
                .ok_or_else(|| ApiError::InvalidRequest("missing subprocess mode".into()))?
                .mode
                .ok_or_else(|| ApiError::InvalidRequest("missing typed subprocess mode".into()))?;

            let mode = match mode {
                proto_api::subprocess_mode::Mode::Command(command) => SubprocessMode::Command {
                    command: command.command,
                    args: command.args,
                },
                proto_api::subprocess_mode::Mode::Script(script) => SubprocessMode::Script {
                    interpreter: script.interpreter,
                    body: script.body,
                    args: script.args,
                },
            };

            mode.validate()
                .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
            Ok(TaskWorkload::Subprocess(SubprocessSpec::new(
                mode,
                convert_env(subprocess.env),
                subprocess.cwd.map(std::path::PathBuf::from),
                Flag::from(subprocess.fail_on_non_zero),
            )))
        }
        proto_api::task_workload::Spec::Wasm(wasm) => {
            validate_builtin_workload_gvk(&api_version, &kind, "Wasm")?;
            if wasm.module.trim().is_empty() {
                return Err(ApiError::InvalidRequest("wasm module path is empty".into()));
            }
            Ok(TaskWorkload::Wasm(WasmSpec::new(
                std::path::PathBuf::from(wasm.module),
                wasm.args,
                convert_env(wasm.env),
            )))
        }
        proto_api::task_workload::Spec::Container(container) => {
            validate_builtin_workload_gvk(&api_version, &kind, "Container")?;
            if container.image.trim().is_empty() {
                return Err(ApiError::InvalidRequest("container image is empty".into()));
            }
            Ok(TaskWorkload::Container(ContainerSpec::new(
                container.image,
                container.command.map(|command| command.items),
                container.args,
                convert_env(container.env),
            )))
        }
        proto_api::task_workload::Spec::Extension(extension) => {
            let spec = extension.spec.ok_or_else(|| {
                ApiError::InvalidRequest("missing extension workload spec".into())
            })?;
            let spec = serde_json::from_slice(&spec.raw).map_err(|error| {
                ApiError::InvalidRequest(format!(
                    "extension workload spec is not valid UTF-8 JSON: {error}"
                ))
            })?;
            ExtensionWorkload::new(api_version, kind, spec)
                .map(TaskWorkload::Extension)
                .map_err(|error| ApiError::InvalidRequest(error.to_string()))
        }
    }
}

fn wire_path_to_proto(field: &str, path: &Path) -> Result<String, ApiError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        ApiError::Internal(format!(
            "handler returned a {field} that is not valid UTF-8"
        ))
    })
}

fn validate_builtin_workload_gvk(
    api_version: &str,
    kind: &str,
    expected_kind: &str,
) -> Result<(), ApiError> {
    if api_version != WORKLOAD_API_VERSION {
        return Err(ApiError::InvalidRequest(format!(
            "unsupported built-in workload apiVersion `{api_version}`"
        )));
    }
    if kind != expected_kind {
        return Err(ApiError::InvalidRequest(format!(
            "workload kind `{kind}` does not match `{expected_kind}` spec"
        )));
    }
    Ok(())
}

fn env_to_proto(env: &TaskEnv) -> Vec<proto_api::KeyValue> {
    env.iter()
        .map(|value| proto_api::KeyValue {
            key: value.key().to_string(),
            value: value.value().to_string(),
        })
        .collect()
}

fn convert_env(values: Vec<proto_api::KeyValue>) -> TaskEnv {
    let mut env = TaskEnv::new();
    for value in values {
        env.push(value.key, value.value);
    }
    env
}
