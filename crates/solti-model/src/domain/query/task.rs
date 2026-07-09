//! Task query builder.
//!
//! [`TaskQuery`] and [`TaskPage`] support filtered, paginated task listing.

use crate::{Slot, TaskPhase};

/// Default page size when the caller does not specify one.
pub const DEFAULT_LIMIT: usize = 100;

/// Hard cap on page size.
///
/// [`TaskQuery::with_limit`] clamps values above this silently;
/// Upstream transports should reject oversized limits explicitly if they expose a wire contract.
pub const MAX_LIMIT: usize = 1000;

/// Query parameters for listing tasks with filtering and pagination.
///
/// An empty `status` filter matches **all** phases (no filtering).
/// Multiple [`with_status`](Self::with_status) calls accumulate with OR semantics.
///
/// ## Example
///
/// ```
/// use solti_model::{TaskPhase, TaskQuery};
///
/// let query = TaskQuery::new()
///     .with_slot("build")
///     .with_active()
///     .with_limit(50)
///     .with_offset(100);
///
/// assert_eq!(query.slot().unwrap().as_str(), "build");
/// assert_eq!(query.limit(), 50);
/// assert!(query.matches_phase(&TaskPhase::Pending));
/// assert!(!query.matches_phase(&TaskPhase::Failed));
/// ```
#[derive(Debug, Clone)]
pub struct TaskQuery {
    status: Vec<TaskPhase>,
    slot: Option<Slot>,
    offset: usize,
    limit: usize,
}

impl Default for TaskQuery {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a paginated task query.
#[derive(Debug, Clone)]
pub struct TaskPage<T> {
    /// Items on this page, at most [`TaskQuery::limit`] entries.
    pub items: Vec<T>,
    /// Total number of items that match the query across all pages.
    pub total: usize,
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
        Self {
            limit: DEFAULT_LIMIT,
            status: Vec::new(),
            slot: None,
            offset: 0,
        }
    }

    /// Filter by slot name.
    #[inline]
    pub fn with_slot(mut self, slot: impl Into<Slot>) -> Self {
        self.slot = Some(slot.into());
        self
    }

    /// Add a phase filter.
    ///
    /// Multiple calls accumulate with OR semantics.
    #[inline]
    pub fn with_status(mut self, status: TaskPhase) -> Self {
        if !self.status.contains(&status) {
            self.status.push(status);
        }
        self
    }

    /// Filter by all active phases: `Pending` and `Running`.
    #[inline]
    pub fn with_active(self) -> Self {
        self.with_status(TaskPhase::Pending)
            .with_status(TaskPhase::Running)
    }

    /// Filter by all terminal phases.
    #[inline]
    pub fn with_terminal(self) -> Self {
        self.with_status(TaskPhase::Succeeded)
            .with_status(TaskPhase::Exhausted)
            .with_status(TaskPhase::Canceled)
            .with_status(TaskPhase::Timeout)
            .with_status(TaskPhase::Failed)
    }

    /// Set page size.
    ///
    /// Values above [`MAX_LIMIT`] are capped.
    #[inline]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.min(MAX_LIMIT);
        self
    }

    /// Set the starting offset for pagination.
    #[inline]
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Return `true` if the given phase passes the status filter.
    ///
    /// An empty filter matches all phases.
    #[inline]
    pub fn matches_phase(&self, phase: &TaskPhase) -> bool {
        self.status.is_empty() || self.status.contains(phase)
    }

    /// Page size limit.
    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Starting offset for pagination.
    #[inline]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Slot filter (if any).
    #[inline]
    pub fn slot(&self) -> Option<&Slot> {
        self.slot.as_ref()
    }

    /// Status filters.
    #[inline]
    pub fn status_filters(&self) -> &[TaskPhase] {
        &self.status
    }
}
