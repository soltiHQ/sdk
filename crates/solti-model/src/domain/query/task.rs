//! Task query builder.
//!
//! [`TaskFilter`], [`TaskQuery`] and [`TaskPage`] support task collections.

use serde::{Deserialize, Serialize};

use crate::{LabelSelector, Labels, ModelError, ModelResult, Slot, Task, TaskId, TaskPhase};

/// Default page size when the caller does not specify one.
pub const DEFAULT_LIMIT: usize = 100;

/// Hard cap on page size.
///
/// [`TaskQuery::with_limit`] clamps values above this silently;
/// Upstream transports should reject oversized limits explicitly if they expose a wire contract.
pub const MAX_LIMIT: usize = 1000;

/// Filters shared by task list and watch operations.
///
/// An empty phase filter matches **all** phases.
/// Multiple [`with_phase`](Self::with_phase) calls accumulate with OR semantics.
/// Slot, label and phase filters are ANDed.
///
/// ## Example
///
/// ```
/// use solti_model::{Slot, TaskFilter, TaskPhase};
///
/// let filter = TaskFilter::new()
///     .with_slot(Slot::new("build").unwrap())
///     .with_active();
///
/// assert_eq!(filter.slot().unwrap().as_str(), "build");
/// assert!(filter.matches_phase(&TaskPhase::Pending));
/// assert!(!filter.matches_phase(&TaskPhase::Failed));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "raw::TaskFilterRaw"
)]
pub struct TaskFilter {
    phases: Vec<TaskPhase>,
    slot: Option<Slot>,
    label_selector: LabelSelector,
}

/// Position of the next page in one Task collection snapshot.
///
/// The cursor is a domain value, not a wire token. Transport layers encode it
/// into their own opaque continuation representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "raw::TaskContinuationRaw"
)]
pub struct TaskContinuation {
    resource_version: String,
    filter: TaskFilter,
    after: TaskId,
}

/// Query parameters for filtered, snapshot-consistent Task listing.
///
/// Filtering is carried by [`TaskFilter`]. Pagination applies only to list
/// operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskQuery {
    filter: TaskFilter,
    limit: usize,
    continuation: Option<TaskContinuation>,
}

impl Default for TaskQuery {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// One page from a Task collection snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage<T> {
    /// Items on this page, at most [`TaskQuery::limit`] entries.
    pub items: Vec<T>,
    /// Opaque collection version captured with this page.
    pub resource_version: String,
    /// Cursor for the next page. `None` means this is the last page.
    pub continuation: Option<TaskContinuation>,
    /// Number of matching items after this page in the same snapshot.
    pub remaining_item_count: usize,
}

impl TaskContinuation {
    /// Create a domain cursor decoded by a transport layer.
    ///
    /// The resource version remains opaque here. The state store validates that
    /// it belongs to a retained snapshot when the query is executed.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `resource_version` is empty or whitespace.
    pub fn new(
        resource_version: impl Into<String>,
        filter: TaskFilter,
        after: TaskId,
    ) -> ModelResult<Self> {
        let resource_version = resource_version.into();
        if resource_version.trim().is_empty() {
            return Err(ModelError::Invalid(
                "continuation resourceVersion must not be empty".into(),
            ));
        }
        Ok(Self {
            resource_version,
            filter,
            after,
        })
    }

    /// Collection snapshot version carried by this cursor.
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    /// Filters fixed by the first page.
    pub fn filter(&self) -> &TaskFilter {
        &self.filter
    }

    /// Last Task name returned before this cursor.
    pub fn after(&self) -> &TaskId {
        &self.after
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskFilterRaw {
        #[serde(default)]
        phases: Vec<TaskPhase>,
        #[serde(default)]
        slot: Option<Slot>,
        #[serde(default)]
        label_selector: LabelSelector,
    }

    impl TryFrom<TaskFilterRaw> for TaskFilter {
        type Error = ModelError;

        fn try_from(raw: TaskFilterRaw) -> Result<Self, Self::Error> {
            raw.label_selector.validate()?;
            let mut filter = Self {
                phases: Vec::new(),
                slot: raw.slot,
                label_selector: raw.label_selector,
            };
            for phase in raw.phases {
                filter = filter.with_phase(phase);
            }
            Ok(filter)
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskContinuationRaw {
        resource_version: String,
        filter: TaskFilter,
        after: TaskId,
    }

    impl TryFrom<TaskContinuationRaw> for TaskContinuation {
        type Error = ModelError;

        fn try_from(raw: TaskContinuationRaw) -> Result<Self, Self::Error> {
            TaskContinuation::new(raw.resource_version, raw.filter, raw.after)
        }
    }
}

impl TaskFilter {
    /// Create an empty filter.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter by slot name.
    #[inline]
    pub fn with_slot(mut self, slot: Slot) -> Self {
        self.slot = Some(slot);
        self
    }

    /// Add a phase filter.
    ///
    /// Multiple calls accumulate with OR semantics.
    #[inline]
    pub fn with_phase(mut self, phase: TaskPhase) -> Self {
        if !self.phases.contains(&phase) {
            self.phases.push(phase);
        }
        self
    }

    /// Add phase filters from an iterator.
    ///
    /// Values are deduplicated and retain OR semantics.
    pub fn with_phases(mut self, phases: impl IntoIterator<Item = TaskPhase>) -> Self {
        for phase in phases {
            self = self.with_phase(phase);
        }
        self
    }

    /// Filter by labels.
    ///
    /// Every selector requirement is ANDed. An empty selector matches all tasks.
    #[inline]
    pub fn with_label_selector(mut self, selector: LabelSelector) -> ModelResult<Self> {
        selector.validate()?;
        self.label_selector = selector;
        Ok(self)
    }

    /// Filter by all active phases: `Pending` and `Running`.
    #[inline]
    pub fn with_active(self) -> Self {
        self.with_phase(TaskPhase::Pending)
            .with_phase(TaskPhase::Running)
    }

    /// Filter by all terminal phases.
    #[inline]
    pub fn with_terminal(self) -> Self {
        self.with_phase(TaskPhase::Succeeded)
            .with_phase(TaskPhase::Exhausted)
            .with_phase(TaskPhase::Canceled)
            .with_phase(TaskPhase::Timeout)
            .with_phase(TaskPhase::Failed)
    }

    /// Return whether a task passes every filter.
    #[inline]
    pub fn matches(&self, task: &Task) -> bool {
        self.slot.as_ref().is_none_or(|slot| slot == task.slot())
            && self.matches_phase(task.phase())
            && self.matches_labels(task.labels())
    }

    /// Return `true` if the given phase passes the phase filter.
    ///
    /// An empty filter matches all phases.
    #[inline]
    pub fn matches_phase(&self, phase: &TaskPhase) -> bool {
        self.phases.is_empty() || self.phases.contains(phase)
    }

    /// Return whether the labels pass the selector.
    #[inline]
    pub fn matches_labels(&self, labels: &Labels) -> bool {
        self.label_selector.matches(labels)
    }

    /// Slot filter (if any).
    #[inline]
    pub fn slot(&self) -> Option<&Slot> {
        self.slot.as_ref()
    }

    /// Phase filters.
    #[inline]
    pub fn phases(&self) -> &[TaskPhase] {
        &self.phases
    }

    /// Label selector.
    #[inline]
    pub fn label_selector(&self) -> &LabelSelector {
        &self.label_selector
    }
}

impl TaskQuery {
    /// Create a new query with default pagination and without filters.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{DEFAULT_LIMIT, TaskPhase, TaskQuery};
    ///
    /// let query = TaskQuery::new();
    /// assert_eq!(query.limit(), DEFAULT_LIMIT);
    /// assert!(query.matches_phase(&TaskPhase::Failed));
    /// ```
    #[inline]
    pub fn new() -> Self {
        Self::from_filter(TaskFilter::new())
    }

    /// Create a query from filters with default pagination.
    #[inline]
    pub fn from_filter(filter: TaskFilter) -> Self {
        Self {
            filter,
            limit: DEFAULT_LIMIT,
            continuation: None,
        }
    }

    /// Filter by slot name.
    #[inline]
    pub fn with_slot(mut self, slot: Slot) -> Self {
        self.filter = self.filter.with_slot(slot);
        self
    }

    /// Add a phase filter.
    ///
    /// Multiple calls accumulate with OR semantics.
    #[inline]
    pub fn with_phase(mut self, phase: TaskPhase) -> Self {
        self.filter = self.filter.with_phase(phase);
        self
    }

    /// Add phase filters from an iterator.
    ///
    /// Values are deduplicated and retain OR semantics.
    #[inline]
    pub fn with_phases(mut self, phases: impl IntoIterator<Item = TaskPhase>) -> Self {
        self.filter = self.filter.with_phases(phases);
        self
    }

    /// Filter by labels.
    ///
    /// Every selector requirement is ANDed. An empty selector matches all tasks.
    #[inline]
    pub fn with_label_selector(mut self, selector: LabelSelector) -> ModelResult<Self> {
        self.filter = self.filter.with_label_selector(selector)?;
        Ok(self)
    }

    /// Filter by all active phases: `Pending` and `Running`.
    #[inline]
    pub fn with_active(mut self) -> Self {
        self.filter = self.filter.with_active();
        self
    }

    /// Filter by all terminal phases.
    #[inline]
    pub fn with_terminal(mut self) -> Self {
        self.filter = self.filter.with_terminal();
        self
    }

    /// Set page size.
    ///
    /// Zero selects [`DEFAULT_LIMIT`]. Values above [`MAX_LIMIT`] are capped.
    #[inline]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = if limit == 0 {
            DEFAULT_LIMIT
        } else {
            limit.min(MAX_LIMIT)
        };
        self
    }

    /// Continue a previously returned collection snapshot.
    #[inline]
    pub fn with_continuation(mut self, continuation: TaskContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// Return whether a task passes every filter.
    #[inline]
    pub fn matches(&self, task: &Task) -> bool {
        self.filter.matches(task)
    }

    /// Return `true` if the given phase passes the phase filter.
    #[inline]
    pub fn matches_phase(&self, phase: &TaskPhase) -> bool {
        self.filter.matches_phase(phase)
    }

    /// Return whether the labels pass the selector.
    #[inline]
    pub fn matches_labels(&self, labels: &Labels) -> bool {
        self.filter.matches_labels(labels)
    }

    /// Page size limit.
    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Continuation cursor, when this is not the first page.
    #[inline]
    pub fn continuation(&self) -> Option<&TaskContinuation> {
        self.continuation.as_ref()
    }

    /// Filters applied before pagination.
    #[inline]
    pub fn filter(&self) -> &TaskFilter {
        &self.filter
    }

    /// Slot filter (if any).
    #[inline]
    pub fn slot(&self) -> Option<&Slot> {
        self.filter.slot()
    }

    /// Phase filters.
    #[inline]
    pub fn phases(&self) -> &[TaskPhase] {
        self.filter.phases()
    }

    /// Label selector.
    #[inline]
    pub fn label_selector(&self) -> &LabelSelector {
        self.filter.label_selector()
    }
}

/// One change emitted by a task watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskWatchEvent {
    /// A task entered the watched collection.
    Added(Task),
    /// A task already in the watched collection changed.
    Modified(Task),
    /// A task left the watched collection or was deleted.
    Deleted(Task),
}

impl TaskWatchEvent {
    /// Resource carried by this event.
    #[inline]
    pub fn object(&self) -> &Task {
        match self {
            Self::Added(task) | Self::Modified(task) | Self::Deleted(task) => task,
        }
    }

    /// Opaque store version of this event.
    #[inline]
    pub fn resource_version(&self) -> &str {
        self.object().metadata().resource_version()
    }

    /// Consume the event and return its resource.
    #[inline]
    pub fn into_object(self) -> Task {
        match self {
            Self::Added(task) | Self::Modified(task) | Self::Deleted(task) => task,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedSpec, TaskSpec, TaskWorkload};

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        let mut labels = Labels::new();
        for (key, value) in pairs {
            labels.insert(*key, *value);
        }
        labels
    }

    #[test]
    fn phases_are_deduplicated_and_matched_with_or_semantics() {
        let query = TaskQuery::new()
            .with_phase(TaskPhase::Pending)
            .with_phase(TaskPhase::Running)
            .with_phase(TaskPhase::Pending);

        assert_eq!(query.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
        assert!(query.matches_phase(&TaskPhase::Pending));
        assert!(query.matches_phase(&TaskPhase::Running));
        assert!(!query.matches_phase(&TaskPhase::Failed));
    }

    #[test]
    fn empty_phase_and_label_filters_match_everything() {
        let query = TaskQuery::new();
        assert!(query.matches_phase(&TaskPhase::Failed));
        assert!(query.matches_labels(&labels(&[("environment", "production")])));
    }

    #[test]
    fn label_selector_is_applied() {
        let query = TaskQuery::new()
            .with_label_selector(
                "environment=production,!tainted"
                    .parse::<LabelSelector>()
                    .unwrap(),
            )
            .unwrap();

        assert!(query.matches_labels(&labels(&[("environment", "production")])));
        assert!(!query.matches_labels(&labels(&[("environment", "development")])));
        assert!(!query.matches_labels(&labels(&[
            ("environment", "production"),
            ("tainted", "true"),
        ])));
    }

    #[test]
    fn query_keeps_filter_separate_from_pagination() {
        let filter = TaskFilter::new()
            .with_slot(Slot::new("build").unwrap())
            .with_phase(TaskPhase::Running);
        let continuation =
            TaskContinuation::new("store:7", filter.clone(), TaskId::new("build-50").unwrap())
                .unwrap();
        let query = TaskQuery::from_filter(filter.clone())
            .with_limit(25)
            .with_continuation(continuation.clone());

        assert_eq!(query.filter(), &filter);
        assert_eq!(query.limit(), 25);
        assert_eq!(query.continuation(), Some(&continuation));
        assert_eq!(continuation.resource_version(), "store:7");
        assert_eq!(continuation.filter(), &filter);
        assert_eq!(continuation.after().as_str(), "build-50");
    }

    #[test]
    fn zero_limit_uses_the_default() {
        assert_eq!(TaskQuery::new().with_limit(0).limit(), DEFAULT_LIMIT);
    }

    #[test]
    fn continuation_construction_rejects_empty_resource_version() {
        assert!(matches!(
            TaskContinuation::new("  ", TaskFilter::new(), TaskId::new("build-50").unwrap(),),
            Err(ModelError::Invalid(_))
        ));
    }

    #[test]
    fn continuation_has_a_strict_serde_roundtrip() {
        let filter = TaskFilter::new()
            .with_slot(Slot::new("build").unwrap())
            .with_phase(TaskPhase::Running)
            .with_label_selector("environment=production".parse().unwrap())
            .unwrap();
        let continuation =
            TaskContinuation::new("store:7", filter, TaskId::new("build-50").unwrap()).unwrap();

        let json = serde_json::to_string(&continuation).unwrap();
        let decoded: TaskContinuation = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, continuation);
        assert!(
            serde_json::from_str::<TaskContinuation>(
                r#"{"resourceVersion":"","filter":{},"after":"build-50"}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<TaskFilter>(
                r#"{"labelSelector":{"matchExpressions":[{"key":"tier","operator":"In","values":[]}]}}"#,
            )
            .is_err()
        );
        assert!(serde_json::from_str::<TaskFilter>(r#"{"unknown":true}"#).is_err());
    }

    #[test]
    fn watch_event_exposes_object_resource_version() {
        let spec = TaskSpec::builder(
            "build",
            TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap()),
            1_000_u64,
        )
        .build()
        .unwrap();
        let mut task = Task::new("build-1", spec).unwrap();
        task.set_resource_version("store:7").unwrap();
        let event = TaskWatchEvent::Modified(task.clone());

        assert_eq!(event.object(), &task);
        assert_eq!(event.resource_version(), "store:7");
        assert_eq!(event.into_object(), task);
    }
}
