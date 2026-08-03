//! # Continuation Tokens
//!
//! HTTP and gRPC expose opaque list continuation strings.
//! The encoded payload contains the domain cursor, snapshot version, and filter.
//!
//! ```text
//! TaskContinuation ── JSON envelope ── URL-safe base64 ──► wire token
//! ```
//!
//! Decoding checks the envelope version.
//! Page validation checks the handler result against the original request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use solti_model::{Task, TaskContinuation, TaskFilter, TaskPage};

use crate::ApiError;

const TOKEN_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContinuationEnvelope {
    version: u8,
    continuation: TaskContinuation,
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

pub(crate) fn validate_page(
    page: &TaskPage<Task>,
    filter: &TaskFilter,
    limit: usize,
) -> Result<(), ApiError> {
    if page.resource_version.trim().is_empty() {
        return Err(invalid_page("resourceVersion is empty"));
    }
    if page.items.len() > limit {
        return Err(invalid_page("item count exceeds the requested limit"));
    }
    if page.items.iter().any(|task| !filter.matches(task)) {
        return Err(invalid_page("an item does not match the requested filters"));
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
            if continuation.filter() != filter {
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

fn invalid_token() -> ApiError {
    ApiError::InvalidRequest("invalid continue token".into())
}

fn invalid_page(message: &str) -> ApiError {
    ApiError::Internal(format!("handler returned an invalid Task page: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{EmbeddedSpec, Slot, TaskId, TaskPhase, TaskSpec, TaskWorkload};

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
    fn page_validation_pins_filter_version_and_cursor() {
        let filter = TaskFilter::new().with_phase(TaskPhase::Pending);
        let continuation =
            TaskContinuation::new("store:2", filter.clone(), TaskId::new("task-1").unwrap())
                .unwrap();
        let page = TaskPage {
            items: vec![task("task-1")],
            resource_version: "store:2".into(),
            continuation: Some(continuation),
            remaining_item_count: 1,
        };

        validate_page(&page, &filter, 1).unwrap();
    }

    #[test]
    fn page_validation_rejects_an_inconsistent_handler_page() {
        let filter = TaskFilter::new();
        let page = TaskPage {
            items: vec![task("task-1")],
            resource_version: "store:2".into(),
            continuation: None,
            remaining_item_count: 1,
        };

        assert!(matches!(
            validate_page(&page, &filter, 1),
            Err(ApiError::Internal(_))
        ));
    }
}
