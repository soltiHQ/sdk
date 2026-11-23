//! # Workload Conversion
//!
//! Every workload travels as its JSON body in `TaskWorkload.extension.spec.raw`.
//! Embedded workloads have no public protobuf representation.

use solti_model::{TaskWorkload, Workload};

use crate::{error::ApiError, proto_api};

/// Also what an old client gets for a typed workload: its field is unknown here, so the
/// workload arrives without a body.
const MISSING_BODY: &str = concat!(
    "missing workload body: set TaskWorkload.extension.spec.raw (the typed subprocess, ",
    "wasm and container fields were removed in Task API contract 0.2)"
);

pub(super) fn workload_to_proto(
    workload: &TaskWorkload,
) -> Result<proto_api::TaskWorkload, ApiError> {
    let routed = match workload {
        TaskWorkload::Routed(routed) => routed,
        TaskWorkload::Embedded(_) => {
            return Err(ApiError::Internal(
                "handler returned an Embedded workload with no wire representation".into(),
            ));
        }
        _ => {
            return Err(ApiError::Internal(
                "handler returned a workload kind with no public wire representation".into(),
            ));
        }
    };
    let raw = serde_json::to_vec(routed.body())
        .map_err(|error| ApiError::Internal(format!("failed to encode workload body: {error}")))?;
    Ok(proto_api::TaskWorkload {
        api_version: routed.api_version().to_owned(),
        kind: routed.kind().to_owned(),
        spec: Some(proto_api::task_workload::Spec::Extension(
            proto_api::ExtensionTask {
                spec: Some(proto_api::RawExtension { raw }),
            },
        )),
    })
}

pub(super) fn convert_task_workload(
    workload: proto_api::TaskWorkload,
) -> Result<TaskWorkload, ApiError> {
    let api_version = workload.api_version;
    let kind = workload.kind;

    let extension = match workload.spec {
        Some(proto_api::task_workload::Spec::Extension(extension)) => extension,
        None => return Err(ApiError::InvalidRequest(MISSING_BODY.into())),
    };
    let raw = extension
        .spec
        .ok_or_else(|| ApiError::InvalidRequest(MISSING_BODY.into()))?
        .raw;
    let body = serde_json::from_slice(&raw).map_err(|error| {
        ApiError::InvalidRequest(format!(
            "workload body in extension.spec.raw is not valid UTF-8 JSON: {error}"
        ))
    })?;
    Workload::new(api_version, kind, body)
        .map(TaskWorkload::Routed)
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))
}

#[cfg(test)]
mod tests {}
