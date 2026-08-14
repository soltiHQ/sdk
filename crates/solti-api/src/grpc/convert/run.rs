//! # Run Conversion
//!
//! Converts domain run history into protobuf response values.
//! Embedded run history has no public wire representation.

use prost::Message as _;
use solti_model::{TaskRun, TaskRunPage};

use super::time::system_time_to_ms;
use crate::error::ApiError;
use crate::proto_api;
use crate::visibility::run_is_visible;

impl TryFrom<TaskRun> for proto_api::TaskRunInfo {
    type Error = ApiError;

    fn try_from(run: TaskRun) -> Result<Self, Self::Error> {
        if !run_is_visible(&run) {
            return Err(ApiError::Internal(
                "handler returned an Embedded task run with no wire representation".into(),
            ));
        }

        let (workload, generation, attempt, phase, started_at, finished_at, error, exit_code) =
            run.into_parts();
        Ok(proto_api::TaskRunInfo {
            workload_api_version: workload.api_version().to_owned(),
            workload_kind: workload.kind().to_owned(),
            generation,
            phase: proto_api::TaskPhase::try_from(phase)? as i32,
            finished_at: finished_at.map(system_time_to_ms).transpose()?,
            started_at: system_time_to_ms(started_at)?,
            exit_code,
            attempt,
            error,
        })
    }
}

/// Converts a validated domain page without transport-size shaping.
fn runs_page_to_proto(page: TaskRunPage) -> Result<proto_api::ListTaskRunsResponse, ApiError> {
    let remaining_item_count = u64::try_from(page.remaining_item_count).map_err(|_| {
        ApiError::Internal("remaining TaskRun count is outside the protobuf range".into())
    })?;
    let continuation = page
        .continuation
        .map(crate::continuation::encode_run)
        .transpose()?
        .unwrap_or_default();
    let runs = page
        .items
        .into_iter()
        .map(proto_api::TaskRunInfo::try_from)
        .collect::<Result<_, _>>()?;
    Ok(proto_api::ListTaskRunsResponse {
        runs,
        resource_version: page.resource_version,
        r#continue: continuation,
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Builds protobuf metadata for one retained run prefix.
fn proto_metadata_for_prefix(
    page: &TaskRunPage,
    keep: usize,
) -> Result<proto_api::ListTaskRunsResponse, ApiError> {
    let (continuation, remaining_item_count) =
        crate::continuation::run_prefix_metadata(page, keep)?;
    let remaining_item_count = u64::try_from(remaining_item_count).map_err(|_| {
        ApiError::Internal("remaining TaskRun count is outside the protobuf range".into())
    })?;
    Ok(proto_api::ListTaskRunsResponse {
        runs: Vec::new(),
        resource_version: page.resource_version.clone(),
        r#continue: continuation
            .map(crate::continuation::encode_run)
            .transpose()?
            .unwrap_or_default(),
        remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
    })
}

/// Converts the largest complete protobuf run prefix within the wire limit.
pub(crate) fn runs_page_to_proto_bounded(
    page: TaskRunPage,
) -> Result<proto_api::ListTaskRunsResponse, ApiError> {
    runs_page_to_proto_with_limit(page, crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES)
}

/// Converts the largest complete protobuf run prefix within `limit` bytes.
fn runs_page_to_proto_with_limit(
    page: TaskRunPage,
    limit: usize,
) -> Result<proto_api::ListTaskRunsResponse, ApiError> {
    let mut run_bytes = 0usize;
    let mut keep = page.items.is_empty().then_some(0);
    for (index, run) in page.items.iter().enumerate() {
        let proto = proto_api::TaskRunInfo::try_from(run.clone())?;
        run_bytes = run_bytes.saturating_add(prost::encoding::message::encoded_len(1, &proto));
        let candidate = index + 1;
        let metadata = proto_metadata_for_prefix(&page, candidate)?;
        if metadata.encoded_len().saturating_add(run_bytes) <= limit {
            keep = Some(candidate);
        }
    }

    let keep = keep.ok_or_else(|| {
        ApiError::ResourceExhausted(format!(
            "the first TaskRun exceeds the {limit}-byte list response limit"
        ))
    })?;
    let page = crate::continuation::retain_run_prefix(page, keep)?;
    let response = runs_page_to_proto(page)?;
    if response.encoded_len() > limit {
        return Err(ApiError::Internal(
            "bounded TaskRun list exceeded its encoded response limit".into(),
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{TaskId, TaskPhase, Uid, WORKLOAD_API_VERSION, WorkloadTypeMeta};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn run_converts_all_fields() {
        let started = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let finished = UNIX_EPOCH + Duration::from_millis(1_700_000_001_500);

        let workload = WorkloadTypeMeta::new("example.io/v1", "DatabaseBackup").unwrap();
        let run = TaskRun::from_parts(
            workload,
            3,
            2,
            TaskPhase::Failed,
            started,
            Some(finished),
            Some("boom".into()),
            Some(137),
        )
        .unwrap();

        let proto = proto_api::TaskRunInfo::try_from(run).unwrap();

        assert_eq!(proto.workload_api_version, "example.io/v1");
        assert_eq!(proto.workload_kind, "DatabaseBackup");
        assert_eq!(proto.attempt, 2);
        assert_eq!(proto.generation, 3);
        assert_eq!(proto.phase, proto_api::TaskPhase::Failed as i32);
        assert_eq!(proto.started_at, 1_700_000_000_000);
        assert_eq!(proto.finished_at, Some(1_700_000_001_500));
        assert_eq!(proto.error.as_deref(), Some("boom"));
        assert_eq!(proto.exit_code, Some(137));
    }

    #[test]
    fn run_active_has_no_finished_timestamp() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let run = TaskRun::starting(1, 1, workload).unwrap();
        let proto = proto_api::TaskRunInfo::try_from(run).unwrap();
        assert_eq!(proto.finished_at, None);
        assert_eq!(proto.exit_code, None);
    }

    #[test]
    fn embedded_run_is_not_exposed() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Embedded").unwrap();
        let run = TaskRun::starting(1, 1, workload).unwrap();

        let error = proto_api::TaskRunInfo::try_from(run).unwrap_err();
        assert!(matches!(error, ApiError::Internal(message) if message.contains("Embedded")));
    }

    #[test]
    fn run_list_stops_at_the_exact_protobuf_boundary() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let page = TaskRunPage {
            items: vec![
                TaskRun::starting(1, 1, workload.clone()).unwrap(),
                TaskRun::from_parts(
                    workload,
                    1,
                    2,
                    TaskPhase::Failed,
                    UNIX_EPOCH + Duration::from_millis(1),
                    Some(UNIX_EPOCH + Duration::from_millis(2)),
                    Some("x".repeat(1_024)),
                    Some(1),
                )
                .unwrap(),
            ],
            task: TaskId::new("task-1").unwrap(),
            task_uid: Uid::new("task-1-uid").unwrap(),
            resource_version: "runs-test:2".into(),
            continuation: None,
            remaining_item_count: 0,
        };
        let first = proto_api::TaskRunInfo::try_from(page.items[0].clone()).unwrap();
        let first_limit = proto_metadata_for_prefix(&page, 1).unwrap().encoded_len()
            + prost::encoding::message::encoded_len(1, &first);

        let response = runs_page_to_proto_with_limit(page.clone(), first_limit).unwrap();
        assert_eq!(response.encoded_len(), first_limit);
        assert_eq!(response.runs.len(), 1);
        assert_eq!(response.remaining_item_count, Some(1));
        let continuation = crate::continuation::decode_run(&response.r#continue).unwrap();
        assert_eq!(continuation.task().as_str(), "task-1");
        assert_eq!(continuation.task_uid().as_str(), "task-1-uid");
        assert_eq!(continuation.after_generation(), 1);
        assert_eq!(continuation.after_attempt(), 1);

        assert!(matches!(
            runs_page_to_proto_with_limit(page, first_limit - 1),
            Err(ApiError::ResourceExhausted(_))
        ));
    }
}
