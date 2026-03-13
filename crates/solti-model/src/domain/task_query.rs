use super::TaskStatus;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

/// Query parameters for listing tasks with filtering and pagination.
#[derive(Debug, Clone)]
pub struct TaskQuery {
    pub slot: Option<String>,
    pub status: Option<TaskStatus>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for TaskQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a paginated task query.
#[derive(Debug, Clone)]
pub struct TaskPage<T> {
    pub items: Vec<T>,
    pub total: usize,
}

impl TaskQuery {
    /// Create a new query with default pagination (`limit=100`, `offset=0`)
    /// and no filters.
    pub fn new() -> Self {
        Self {
            slot: None,
            status: None,
            limit: DEFAULT_LIMIT,
            offset: 0,
        }
    }

    /// Filter by slot name.
    pub fn with_slot(mut self, slot: impl Into<String>) -> Self {
        self.slot = Some(slot.into());
        self
    }

    /// Filter by task status.
    pub fn with_status(mut self, status: TaskStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// Set page size. Capped at `1000`.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.min(MAX_LIMIT);
        self
    }

    /// Set the starting offset for pagination.
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }
}
