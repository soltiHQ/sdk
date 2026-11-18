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
mod tests {
    use super::*;
    use solti_model::EmbeddedSpec;

    fn extension(api_version: &str, kind: &str, raw: &[u8]) -> proto_api::TaskWorkload {
        proto_api::TaskWorkload {
            api_version: api_version.into(),
            kind: kind.into(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask {
                    spec: Some(proto_api::RawExtension { raw: raw.to_vec() }),
                },
            )),
        }
    }

    fn invalid_request(error: ApiError) -> String {
        match error {
            ApiError::InvalidRequest(message) => message,
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn grpc_extension_round_trips_a_routed_workload() {
        let expected = serde_json::json!({
            "enabled": true,
            "exactInteger": 9_007_199_254_740_993_u64,
            "limits": { "cpu": 2.5, "memory": null },
            "names": ["primary", "replica"]
        });
        let workload = TaskWorkload::Routed(
            Workload::new("tasks.example.io/v1", "Echo", expected.clone()).unwrap(),
        );

        let proto = workload_to_proto(&workload).unwrap();
        assert_eq!(proto.api_version, "tasks.example.io/v1");
        assert_eq!(proto.kind, "Echo");
        let Some(proto_api::task_workload::Spec::Extension(extension)) = &proto.spec else {
            panic!("a routed workload must travel as an extension");
        };
        let raw = &extension.spec.as_ref().unwrap().raw;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(raw).unwrap(),
            expected
        );
        assert!(
            std::str::from_utf8(raw)
                .unwrap()
                .contains("9007199254740993"),
            "the exact integer digits must reach the wire"
        );

        assert_eq!(convert_task_workload(proto).unwrap(), workload);
    }

    /// The bytes of a `TaskWorkload` as an old client sends them: `subprocess`
    /// (field 3) set, no `extension`. The field no longer exists, so it is unknown.
    fn old_client_workload_bytes() -> Vec<u8> {
        use prost::Message as _;

        let mut bytes = proto_api::TaskWorkload {
            api_version: "exec.solti.io/v1".into(),
            kind: "Subprocess".into(),
            spec: None,
        }
        .encode_to_vec();
        // Field 3, wire type 2 (length-delimited), holding a short message.
        bytes.extend_from_slice(&[0x1A, 0x04, 0x0A, 0x02, b'l', b's']);
        bytes
    }

    #[test]
    fn grpc_typed_fields_from_an_old_client_are_refused_as_a_missing_body() {
        use prost::Message as _;

        let workload = proto_api::TaskWorkload::decode(old_client_workload_bytes().as_slice())
            .expect("an unknown field is skipped, not an error");
        assert!(workload.spec.is_none(), "field 3 must not select a variant");

        let message = invalid_request(convert_task_workload(workload).unwrap_err());
        assert!(
            message.contains("extension.spec.raw"),
            "the refusal must name the body field: {message}"
        );
        assert!(
            message.contains("removed in Task API contract 0.2"),
            "the refusal must say that the typed fields were removed: {message}"
        );
    }

    #[test]
    fn grpc_refuses_a_missing_or_non_object_body() {
        let missing = concat!(
            "missing workload body: set TaskWorkload.extension.spec.raw (the typed subprocess, ",
            "wasm and container fields were removed in Task API contract 0.2)"
        );

        let no_oneof = proto_api::TaskWorkload {
            api_version: "tasks.example.io/v1".into(),
            kind: "Echo".into(),
            spec: None,
        };
        assert_eq!(
            invalid_request(convert_task_workload(no_oneof).unwrap_err()),
            missing
        );

        let no_raw_message = proto_api::TaskWorkload {
            api_version: "tasks.example.io/v1".into(),
            kind: "Echo".into(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask { spec: None },
            )),
        };
        assert_eq!(
            invalid_request(convert_task_workload(no_raw_message).unwrap_err()),
            missing
        );

        for raw in [&b""[..], b"not json", b"[1, 2]", b"\"text\"", b"null", b"7"] {
            let message = invalid_request(
                convert_task_workload(extension("tasks.example.io/v1", "Echo", raw)).unwrap_err(),
            );
            assert!(
                message.contains("workload body") || message.contains("JSON object"),
                "{raw:?} was refused with an unrelated message: {message}"
            );
        }
    }

    #[test]
    fn embedded_stays_refused_on_both_transports_grpc() {
        let error = convert_task_workload(extension(
            "solti.io/v1",
            "Embedded",
            br#"{"revision":"test-v1"}"#,
        ))
        .unwrap_err();
        assert!(invalid_request(error).contains("reserved"));

        let embedded = TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap());
        assert!(matches!(
            workload_to_proto(&embedded),
            Err(ApiError::Internal(message)) if message.contains("Embedded")
        ));
    }
}
