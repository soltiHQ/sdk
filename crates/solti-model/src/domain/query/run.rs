//! # TaskRun query
//!
//! [`TaskRunQuery`] selects one bounded page from a retained TaskRun snapshot.
//! [`TaskRunContinuation`] fixes the Task name, UID, snapshot, and last run.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{ModelError, ModelResult, TaskId, TaskRun, Uid};

/// Default TaskRun page size.
pub const DEFAULT_TASK_RUN_LIMIT: usize = 100;

/// Hard cap on TaskRun page size.
pub const MAX_TASK_RUN_LIMIT: usize = 1000;

/// Default serialized JSON budget for TaskRun items in one collection page.
///
/// Transport envelopes and framing are accounted separately.
pub const MAX_TASK_RUN_PAGE_ITEM_BYTES: usize = 4 * 1024 * 1024;

/// Position of the next page in one TaskRun collection snapshot.
///
/// The cursor is a domain value. Transport layers encode it as an opaque token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "raw::TaskRunContinuationRaw"
)]
pub struct TaskRunContinuation {
    resource_version: String,
    task: TaskId,
    task_uid: Uid,
    after_generation: u64,
    after_attempt: u32,
}

/// Query parameters for one bounded TaskRun snapshot page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunQuery {
    limit: usize,
    item_byte_limit: NonZeroUsize,
    continuation: Option<TaskRunContinuation>,
}

/// One complete-item prefix from a TaskRun collection snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunPage {
    /// Runs on this page.
    pub items: Vec<TaskRun>,
    /// Task name fixed by this snapshot.
    pub task: TaskId,
    /// Task UID fixed by this snapshot.
    pub task_uid: Uid,
    /// Opaque TaskRun collection snapshot version.
    pub resource_version: String,
    /// Cursor for the next page.
    ///
    /// `None` means this is the last page.
    pub continuation: Option<TaskRunContinuation>,
    /// Number of visible runs after this page in the same snapshot.
    pub remaining_item_count: usize,
}

impl TaskRunContinuation {
    /// Creates a TaskRun continuation.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the resource version is empty or
    /// the run identity contains zero.
    pub fn new(
        resource_version: impl Into<String>,
        task: TaskId,
        task_uid: Uid,
        after_generation: u64,
        after_attempt: u32,
    ) -> ModelResult<Self> {
        let resource_version = resource_version.into();
        if resource_version.trim().is_empty() {
            return Err(ModelError::Invalid(
                "TaskRun continuation resourceVersion must not be empty".into(),
            ));
        }
        if after_generation == 0 {
            return Err(ModelError::Invalid(
                "TaskRun continuation generation must be greater than zero".into(),
            ));
        }
        if after_attempt == 0 {
            return Err(ModelError::Invalid(
                "TaskRun continuation attempt must be greater than zero".into(),
            ));
        }
        Ok(Self {
            resource_version,
            task,
            task_uid,
            after_generation,
            after_attempt,
        })
    }

    /// TaskRun collection snapshot version.
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    /// Task name fixed by the first page.
    pub fn task(&self) -> &TaskId {
        &self.task
    }

    /// Task UID fixed by the first page.
    pub fn task_uid(&self) -> &Uid {
        &self.task_uid
    }

    /// Generation of the last returned run.
    pub fn after_generation(&self) -> u64 {
        self.after_generation
    }

    /// Attempt of the last returned run.
    pub fn after_attempt(&self) -> u32 {
        self.after_attempt
    }
}

impl Default for TaskRunQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRunQuery {
    /// Creates a first-page query with default limits.
    pub fn new() -> Self {
        Self {
            limit: DEFAULT_TASK_RUN_LIMIT,
            item_byte_limit: NonZeroUsize::new(MAX_TASK_RUN_PAGE_ITEM_BYTES)
                .expect("the maximum TaskRun page item byte limit is positive"),
            continuation: None,
        }
    }

    /// Sets the page size.
    ///
    /// Zero selects [`DEFAULT_TASK_RUN_LIMIT`]. Values above
    /// [`MAX_TASK_RUN_LIMIT`] are capped.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = if limit == 0 {
            DEFAULT_TASK_RUN_LIMIT
        } else {
            limit.min(MAX_TASK_RUN_LIMIT)
        };
        self
    }

    /// Limits compact JSON bytes carried by complete TaskRun items.
    ///
    /// The budget includes commas between items. It excludes the collection
    /// envelope and transport framing. The first item is returned even when it
    /// exceeds this budget for native transport measurement.
    pub fn with_item_byte_limit(mut self, limit: NonZeroUsize) -> Self {
        self.item_byte_limit = NonZeroUsize::new(limit.get().min(MAX_TASK_RUN_PAGE_ITEM_BYTES))
            .expect("the maximum TaskRun page item byte limit is positive");
        self
    }

    /// Continues a previously returned TaskRun snapshot.
    pub fn with_continuation(mut self, continuation: TaskRunContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// Page size limit.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Compact JSON byte limit for page items.
    pub fn item_byte_limit(&self) -> NonZeroUsize {
        self.item_byte_limit
    }

    /// Continuation cursor, when present.
    pub fn continuation(&self) -> Option<&TaskRunContinuation> {
        self.continuation.as_ref()
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskRunContinuationRaw {
        resource_version: String,
        task: TaskId,
        task_uid: Uid,
        after_generation: u64,
        after_attempt: u32,
    }

    impl TryFrom<TaskRunContinuationRaw> for TaskRunContinuation {
        type Error = ModelError;

        fn try_from(raw: TaskRunContinuationRaw) -> Result<Self, Self::Error> {
            TaskRunContinuation::new(
                raw.resource_version,
                raw.task,
                raw.task_uid,
                raw.after_generation,
                raw.after_attempt,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn continuation() -> TaskRunContinuation {
        TaskRunContinuation::new(
            "runs-store:7",
            TaskId::new("build-1").unwrap(),
            Uid::new("01K0QWERTYUIOPASDFGHJKLZX2").unwrap(),
            3,
            2,
        )
        .unwrap()
    }

    #[test]
    fn query_applies_count_and_byte_defaults_and_caps() {
        let query = TaskRunQuery::new();
        assert_eq!(query.limit(), DEFAULT_TASK_RUN_LIMIT);
        assert_eq!(query.item_byte_limit().get(), MAX_TASK_RUN_PAGE_ITEM_BYTES);

        assert_eq!(
            TaskRunQuery::new().with_limit(usize::MAX).limit(),
            MAX_TASK_RUN_LIMIT
        );
        assert_eq!(
            TaskRunQuery::new().with_limit(0).limit(),
            DEFAULT_TASK_RUN_LIMIT
        );
        assert_eq!(
            TaskRunQuery::new()
                .with_item_byte_limit(NonZeroUsize::new(usize::MAX).unwrap())
                .item_byte_limit()
                .get(),
            MAX_TASK_RUN_PAGE_ITEM_BYTES
        );
    }

    #[test]
    fn continuation_has_a_strict_serde_roundtrip() {
        let continuation = continuation();
        let json = serde_json::to_string(&continuation).unwrap();
        let decoded: TaskRunContinuation = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, continuation);
        assert_eq!(decoded.task().as_str(), "build-1");
        assert_eq!(decoded.after_generation(), 3);
        assert_eq!(decoded.after_attempt(), 2);
        assert!(
            serde_json::from_str::<TaskRunContinuation>(
                r#"{"resourceVersion":"","task":"build-1","taskUid":"01K0QWERTYUIOPASDFGHJKLZX2","afterGeneration":3,"afterAttempt":2}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<TaskRunContinuation>(
                r#"{"resourceVersion":"runs-store:7","task":"build-1","taskUid":"01K0QWERTYUIOPASDFGHJKLZX2","afterGeneration":0,"afterAttempt":2}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<TaskRunContinuation>(
                r#"{"resourceVersion":"runs-store:7","task":"build-1","taskUid":"01K0QWERTYUIOPASDFGHJKLZX2","afterGeneration":3,"afterAttempt":2,"unknown":true}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn query_carries_the_continuation() {
        let continuation = continuation();
        let query = TaskRunQuery::new()
            .with_limit(17)
            .with_item_byte_limit(NonZeroUsize::new(4096).unwrap())
            .with_continuation(continuation.clone());

        assert_eq!(query.limit(), 17);
        assert_eq!(query.item_byte_limit().get(), 4096);
        assert_eq!(query.continuation(), Some(&continuation));
    }
}
