//! # Continuation Tokens
//!
//! HTTP and gRPC expose opaque Task and TaskRun continuation strings.
//! A Task token fixes its filter and last Task name.
//! A TaskRun token fixes its Task name, Task UID, and last run identity.
//!
//! ```text
//! domain continuation ── JSON envelope ── URL-safe base64 ──► wire token
//! ```
//!
//! Decoding checks the envelope version.
//! Page validation checks the handler result against the original request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use solti_model::{
    Task, TaskContinuation, TaskFilter, TaskId, TaskPage, TaskQuery, TaskRunContinuation,
    TaskRunPage, TaskRunQuery,
};

use crate::ApiError;

const TOKEN_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContinuationEnvelope {
    version: u8,
    continuation: TaskContinuation,
}

/// Versioned wire envelope for a TaskRun continuation.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunContinuationEnvelope {
    /// Token format version.
    version: u8,
    /// Domain continuation carried by this token.
    continuation: TaskRunContinuation,
}

pub(crate) fn encode(continuation: TaskContinuation) -> Result<String, ApiError> {
    let payload = serde_json::to_vec(&ContinuationEnvelope {
        version: TOKEN_VERSION,
        continuation,
    })
    .map_err(|error| ApiError::Internal(format!("encode continuation token: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

pub(crate) fn decode(token: &str) -> Result<TaskContinuation, ApiError> {
    let payload = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid_token())?;
    let envelope: ContinuationEnvelope =
        serde_json::from_slice(&payload).map_err(|_| invalid_token())?;
    if envelope.version != TOKEN_VERSION {
        return Err(invalid_token());
    }
    Ok(envelope.continuation)
}

/// Rejects a Task token whose filter differs from the current request.
pub(crate) fn validate_continuation_filter(
    continuation: &TaskContinuation,
    filter: &TaskFilter,
) -> Result<(), ApiError> {
    if continuation.filter() != filter {
        return Err(ApiError::InvalidRequest(
            "continue token filters differ from the request".into(),
        ));
    }
    Ok(())
}

/// Encodes a TaskRun continuation as an opaque wire token.
pub(crate) fn encode_run(continuation: TaskRunContinuation) -> Result<String, ApiError> {
    let payload = serde_json::to_vec(&RunContinuationEnvelope {
        version: TOKEN_VERSION,
        continuation,
    })
    .map_err(|error| ApiError::Internal(format!("encode run continuation token: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

/// Decodes and validates a TaskRun continuation wire token.
pub(crate) fn decode_run(token: &str) -> Result<TaskRunContinuation, ApiError> {
    let payload = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid_token())?;
    let envelope: RunContinuationEnvelope =
        serde_json::from_slice(&payload).map_err(|_| invalid_token())?;
    if envelope.version != TOKEN_VERSION {
        return Err(invalid_token());
    }
    Ok(envelope.continuation)
}

/// Rejects a TaskRun token that belongs to another request path.
pub(crate) fn validate_run_continuation_task(
    continuation: &TaskRunContinuation,
    task: &TaskId,
) -> Result<(), ApiError> {
    if continuation.task() != task {
        return Err(ApiError::InvalidRequest(
            "continue token belongs to a different Task".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_page(page: &TaskPage<Task>, query: &TaskQuery) -> Result<(), ApiError> {
    if page.resource_version.trim().is_empty() {
        return Err(invalid_page("resourceVersion is empty"));
    }
    if page.items.len() > query.limit() {
        return Err(invalid_page("item count exceeds the requested limit"));
    }
    if page.items.iter().any(|task| !query.filter().matches(task)) {
        return Err(invalid_page("an item does not match the requested filters"));
    }
    if let Some(requested) = query.continuation() {
        if requested.filter() != query.filter() {
            return Err(invalid_page(
                "requested continuation filters differ from the query",
            ));
        }
        if requested.resource_version() != page.resource_version {
            return Err(invalid_page(
                "resourceVersion differs from the requested continuation",
            ));
        }
        if page
            .items
            .iter()
            .any(|task| task.name() == requested.after())
        {
            return Err(invalid_page(
                "an item repeats the requested continuation cursor",
            ));
        }
    }

    match (&page.continuation, page.remaining_item_count) {
        (None, 0) => Ok(()),
        (None, _) => Err(invalid_page(
            "remaining items were reported without a continuation",
        )),
        (Some(_), 0) => Err(invalid_page(
            "a continuation was returned without remaining items",
        )),
        (Some(continuation), _) => {
            if continuation.resource_version() != page.resource_version {
                return Err(invalid_page(
                    "continuation resourceVersion differs from the page",
                ));
            }
            if continuation.filter() != query.filter() {
                return Err(invalid_page("continuation filters differ from the request"));
            }
            if page
                .items
                .last()
                .is_none_or(|task| task.name() != continuation.after())
            {
                return Err(invalid_page(
                    "continuation cursor is not the last page item",
                ));
            }
            Ok(())
        }
    }
}

/// Calculates continuation metadata for one retained item prefix.
pub(crate) fn prefix_metadata(
    page: &TaskPage<Task>,
    filter: &TaskFilter,
    keep: usize,
) -> Result<(Option<TaskContinuation>, usize), ApiError> {
    if keep > page.items.len() {
        return Err(invalid_page("a response prefix exceeds the handler page"));
    }
    if keep == page.items.len() {
        return Ok((page.continuation.clone(), page.remaining_item_count));
    }
    if keep == 0 {
        return Err(ApiError::ResourceExhausted(format!(
            "the first Task exceeds the {limit}-byte list response limit",
            limit = crate::MAX_TASK_LIST_RESPONSE_BYTES,
        )));
    }

    let remaining_item_count = page
        .remaining_item_count
        .checked_add(page.items.len() - keep)
        .ok_or_else(|| ApiError::Internal("remaining task count overflow".into()))?;
    let after = page.items[keep - 1].name().clone();
    let continuation = TaskContinuation::new(page.resource_version.clone(), filter.clone(), after)
        .map_err(|error| ApiError::Internal(format!("build response continuation: {error}")))?;
    Ok((Some(continuation), remaining_item_count))
}

/// Retains one item prefix and rewrites its exact continuation metadata.
pub(crate) fn retain_prefix(
    mut page: TaskPage<Task>,
    filter: &TaskFilter,
    keep: usize,
) -> Result<TaskPage<Task>, ApiError> {
    let (continuation, remaining_item_count) = prefix_metadata(&page, filter, keep)?;
    page.items.truncate(keep);
    page.continuation = continuation;
    page.remaining_item_count = remaining_item_count;
    Ok(page)
}

/// Validates a handler TaskRun page against its request contract.
pub(crate) fn validate_run_page(
    page: &TaskRunPage,
    task: &TaskId,
    query: &TaskRunQuery,
) -> Result<(), ApiError> {
    if page.task != *task {
        return Err(invalid_run_page("task differs from the request"));
    }
    if page.resource_version.trim().is_empty() {
        return Err(invalid_run_page("resourceVersion is empty"));
    }
    if page.items.len() > query.limit() {
        return Err(invalid_run_page("item count exceeds the requested limit"));
    }
    if page
        .items
        .iter()
        .any(|run| !crate::visibility::run_is_visible(run))
    {
        return Err(invalid_run_page(
            "an item has no public wire representation",
        ));
    }
    if page.items.windows(2).any(|pair| {
        (pair[0].generation(), pair[0].attempt()) >= (pair[1].generation(), pair[1].attempt())
    }) {
        return Err(invalid_run_page(
            "items are not strictly ordered by generation and attempt",
        ));
    }

    if let Some(requested) = query.continuation() {
        if requested.resource_version() != page.resource_version {
            return Err(invalid_run_page(
                "resourceVersion differs from the requested continuation",
            ));
        }
        if requested.task() != &page.task || requested.task_uid() != &page.task_uid {
            return Err(invalid_run_page(
                "task identity differs from the requested continuation",
            ));
        }
        if page.items.first().is_some_and(|run| {
            (run.generation(), run.attempt())
                <= (requested.after_generation(), requested.after_attempt())
        }) {
            return Err(invalid_run_page(
                "the first item does not follow the requested cursor",
            ));
        }
    }

    match (&page.continuation, page.remaining_item_count) {
        (None, 0) => Ok(()),
        (None, _) => Err(invalid_run_page(
            "remaining items were reported without a continuation",
        )),
        (Some(_), 0) => Err(invalid_run_page(
            "a continuation was returned without remaining items",
        )),
        (Some(continuation), _) => {
            if continuation.resource_version() != page.resource_version {
                return Err(invalid_run_page(
                    "continuation resourceVersion differs from the page",
                ));
            }
            if continuation.task() != &page.task || continuation.task_uid() != &page.task_uid {
                return Err(invalid_run_page(
                    "continuation task identity differs from the page",
                ));
            }
            if page.items.last().is_none_or(|run| {
                (run.generation(), run.attempt())
                    != (
                        continuation.after_generation(),
                        continuation.after_attempt(),
                    )
            }) {
                return Err(invalid_run_page(
                    "continuation cursor is not the last page item",
                ));
            }
            Ok(())
        }
    }
}

/// Calculates TaskRun continuation metadata for one retained item prefix.
pub(crate) fn run_prefix_metadata(
    page: &TaskRunPage,
    keep: usize,
) -> Result<(Option<TaskRunContinuation>, usize), ApiError> {
    if keep > page.items.len() {
        return Err(invalid_run_page(
            "a response prefix exceeds the handler page",
        ));
    }
    if keep == page.items.len() {
        return Ok((page.continuation.clone(), page.remaining_item_count));
    }
    if keep == 0 {
        return Err(ApiError::ResourceExhausted(format!(
            "the first TaskRun exceeds the {limit}-byte list response limit",
            limit = crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES,
        )));
    }

    let remaining_item_count = page
        .remaining_item_count
        .checked_add(page.items.len() - keep)
        .ok_or_else(|| ApiError::Internal("remaining TaskRun count overflow".into()))?;
    let last = &page.items[keep - 1];
    let continuation = TaskRunContinuation::new(
        page.resource_version.clone(),
        page.task.clone(),
        page.task_uid.clone(),
        last.generation(),
        last.attempt(),
    )
    .map_err(|error| ApiError::Internal(format!("build run response continuation: {error}")))?;
    Ok((Some(continuation), remaining_item_count))
}

/// Retains one TaskRun prefix and rewrites its exact continuation metadata.
pub(crate) fn retain_run_prefix(
    mut page: TaskRunPage,
    keep: usize,
) -> Result<TaskRunPage, ApiError> {
    let (continuation, remaining_item_count) = run_prefix_metadata(&page, keep)?;
    page.items.truncate(keep);
    page.continuation = continuation;
    page.remaining_item_count = remaining_item_count;
    Ok(page)
}

fn invalid_token() -> ApiError {
    ApiError::InvalidRequest("invalid continue token".into())
}

fn invalid_page(message: &str) -> ApiError {
    ApiError::Internal(format!("handler returned an invalid Task page: {message}"))
}

fn invalid_run_page(message: &str) -> ApiError {
    ApiError::Internal(format!(
        "handler returned an invalid TaskRun page: {message}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        EmbeddedSpec, Slot, TaskId, TaskPhase, TaskRun, TaskSpec, TaskWorkload, Uid,
        WORKLOAD_API_VERSION, WorkloadTypeMeta,
    };

    fn task(name: &str) -> Task {
        let spec = TaskSpec::builder(
            "slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap()),
            1_000_u64,
        )
        .build()
        .unwrap();
        Task::new(name, spec).unwrap()
    }

    fn run(attempt: u32) -> TaskRun {
        TaskRun::starting(
            1,
            attempt,
            WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn roundtrip_preserves_snapshot_filter_and_cursor() {
        let filter = TaskFilter::new()
            .with_slot(Slot::new("primary").unwrap())
            .with_phase(TaskPhase::Running);
        let continuation =
            TaskContinuation::new("store:42", filter, TaskId::new("task-9").unwrap()).unwrap();

        let decoded = decode(&encode(continuation.clone()).unwrap()).unwrap();

        assert_eq!(decoded, continuation);
    }

    #[test]
    fn malformed_and_unknown_versions_are_rejected() {
        assert!(decode("not-base64!").is_err());

        let token = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "continuation": {
                    "resourceVersion": "store:1",
                    "filter": {},
                    "after": "task-1"
                }
            }))
            .unwrap(),
        );
        assert!(decode(&token).is_err());
    }

    #[test]
    fn run_roundtrip_preserves_snapshot_task_uid_and_cursor() {
        let continuation = TaskRunContinuation::new(
            "runs-store:42",
            TaskId::new("task-9").unwrap(),
            Uid::new("task-9-uid").unwrap(),
            3,
            2,
        )
        .unwrap();

        let decoded = decode_run(&encode_run(continuation.clone()).unwrap()).unwrap();

        assert_eq!(decoded, continuation);
        validate_run_continuation_task(&decoded, &TaskId::new("task-9").unwrap()).unwrap();
        assert!(matches!(
            validate_run_continuation_task(&decoded, &TaskId::new("task-10").unwrap()),
            Err(ApiError::InvalidRequest(_))
        ));
    }

    #[test]
    fn page_validation_pins_filter_version_and_cursor() {
        let filter = TaskFilter::new().with_phase(TaskPhase::Pending);
        let query = TaskQuery::from_filter(filter.clone()).with_limit(1);
        let continuation =
            TaskContinuation::new("store:2", filter.clone(), TaskId::new("task-1").unwrap())
                .unwrap();
        let page = TaskPage {
            items: vec![task("task-1")],
            resource_version: "store:2".into(),
            continuation: Some(continuation),
            remaining_item_count: 1,
        };

        validate_page(&page, &query).unwrap();
    }

    #[test]
    fn page_validation_rejects_an_inconsistent_handler_page() {
        let filter = TaskFilter::new();
        let query = TaskQuery::from_filter(filter);
        let page = TaskPage {
            items: vec![task("task-1")],
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 1,
        };

        assert!(matches!(
            validate_page(&page, &query),
            Err(ApiError::Internal(_))
        ));
    }

    #[test]
    fn continuation_filter_validation_rejects_a_changed_request_filter() {
        let continuation = TaskContinuation::new(
            "store:2",
            TaskFilter::new().with_phase(TaskPhase::Pending),
            TaskId::new("task-1").unwrap(),
        )
        .unwrap();
        let changed_filter = TaskFilter::new().with_phase(TaskPhase::Running);

        assert!(matches!(
            validate_continuation_filter(&continuation, &changed_filter),
            Err(ApiError::InvalidRequest(_))
        ));

        let query = TaskQuery::from_filter(changed_filter).with_continuation(continuation);
        let page = TaskPage {
            items: Vec::new(),
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 0,
        };
        assert!(matches!(
            validate_page(&page, &query),
            Err(ApiError::Internal(_))
        ));
    }

    #[test]
    fn continuation_page_validation_accepts_the_requested_snapshot() {
        let filter = TaskFilter::new();
        let requested =
            TaskContinuation::new("store:2", filter.clone(), TaskId::new("task-1").unwrap())
                .unwrap();
        let query = TaskQuery::from_filter(filter)
            .with_limit(1)
            .with_continuation(requested);
        let page = TaskPage {
            items: vec![task("task-2")],
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 0,
        };

        validate_page(&page, &query).unwrap();
    }

    #[test]
    fn continuation_page_validation_rejects_another_snapshot_and_cursor_replay() {
        let filter = TaskFilter::new();
        let requested =
            TaskContinuation::new("store:2", filter.clone(), TaskId::new("task-1").unwrap())
                .unwrap();
        let query = TaskQuery::from_filter(filter)
            .with_limit(1)
            .with_continuation(requested);
        let another_snapshot = TaskPage {
            items: vec![task("task-2")],
            resource_version: "store:3".into(),
            continuation: None,
            remaining_item_count: 0,
        };
        let cursor_replay = TaskPage {
            items: vec![task("task-1")],
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 0,
        };

        assert!(matches!(
            validate_page(&another_snapshot, &query),
            Err(ApiError::Internal(_))
        ));
        assert!(matches!(
            validate_page(&cursor_replay, &query),
            Err(ApiError::Internal(_))
        ));
    }

    #[test]
    fn retained_prefix_rewrites_cursor_and_exact_remaining_count() {
        let filter = TaskFilter::new();
        let query = TaskQuery::from_filter(filter.clone()).with_limit(2);
        let source_continuation =
            TaskContinuation::new("store:2", filter.clone(), TaskId::new("task-2").unwrap())
                .unwrap();
        let page = TaskPage {
            items: vec![task("task-1"), task("task-2")],
            resource_version: "store:2".into(),
            continuation: Some(source_continuation),
            remaining_item_count: 3,
        };
        validate_page(&page, &query).unwrap();

        let page = retain_prefix(page, &filter, 1).unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.remaining_item_count, 4);
        let continuation = page.continuation.unwrap();
        assert_eq!(continuation.resource_version(), "store:2");
        assert_eq!(continuation.filter(), &filter);
        assert_eq!(continuation.after().as_str(), "task-1");
    }

    #[test]
    fn retained_run_prefix_extends_the_source_remaining_count() {
        let task = TaskId::new("task-1").unwrap();
        let task_uid = Uid::new("task-1-uid").unwrap();
        let source_continuation =
            TaskRunContinuation::new("runs-store:2", task.clone(), task_uid.clone(), 1, 2).unwrap();
        let page = TaskRunPage {
            items: vec![run(1), run(2)],
            task: task.clone(),
            task_uid,
            resource_version: "runs-store:2".into(),
            continuation: Some(source_continuation),
            remaining_item_count: 3,
        };
        validate_run_page(&page, &task, &TaskRunQuery::new().with_limit(2)).unwrap();

        let page = retain_run_prefix(page, 1).unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.remaining_item_count, 4);
        let continuation = page.continuation.unwrap();
        assert_eq!(continuation.resource_version(), "runs-store:2");
        assert_eq!(continuation.task(), &task);
        assert_eq!(continuation.after_generation(), 1);
        assert_eq!(continuation.after_attempt(), 1);
    }
}
