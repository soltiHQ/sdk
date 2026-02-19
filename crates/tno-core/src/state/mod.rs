mod subscriber;
pub use subscriber::StateSubscriber;

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::SystemTime,
};

use tno_model::{Slot, TaskId, TaskInfo, TaskPage, TaskQuery, TaskStatus};

/// In-memory task state storage.
#[derive(Clone)]
pub struct TaskState {
    inner: Arc<RwLock<TaskStateInner>>,
}

struct TaskStateInner {
    /// Tasks indexed by TaskId.
    tasks: HashMap<TaskId, TaskInfo>,
    /// Index: slot -> list of task IDs in that slot.
    by_slot: HashMap<Slot, Vec<TaskId>>,
}

impl TaskState {
    /// Create empty task state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TaskStateInner {
                tasks: HashMap::new(),
                by_slot: HashMap::new(),
            })),
        }
    }

    /// Register a new task (called on TaskAdded event).
    pub fn add_task(&self, id: TaskId, slot: Slot) {
        let mut inner = self.inner.write().unwrap();

        let now = SystemTime::now();
        let info = TaskInfo {
            id: id.clone(),
            slot: slot.clone(),
            status: TaskStatus::Pending,
            attempt: 0,
            created_at: now,
            updated_at: now,
            error: None,
        };

        inner.tasks.insert(id.clone(), info);
        inner.by_slot.entry(slot).or_default().push(id);
    }

    /// Update task status (called on state transition events).
    pub fn update_status(&self, id: &TaskId, status: TaskStatus, error: Option<String>) {
        let mut inner = self.inner.write().unwrap();

        if let Some(info) = inner.tasks.get_mut(id) {
            info.status = status;
            info.updated_at = SystemTime::now();
            if let Some(err) = error {
                info.error = Some(err);
            }
        }
    }

    /// Increment attempt counter (called on TaskStarting event).
    pub fn increment_attempt(&self, id: &TaskId) {
        let mut inner = self.inner.write().unwrap();

        if let Some(info) = inner.tasks.get_mut(id) {
            info.attempt += 1;
            info.updated_at = SystemTime::now();
        }
    }

    /// Remove task from state (called on TaskRemoved event).
    pub fn remove_task(&self, id: &TaskId) {
        let mut inner = self.inner.write().unwrap();

        if let Some(info) = inner.tasks.remove(id)
            && let Some(ids) = inner.by_slot.get_mut(&info.slot)
        {
            ids.retain(|task_id| task_id != id);
        }
    }

    /// Get task info by ID.
    pub fn get(&self, id: &TaskId) -> Option<TaskInfo> {
        let inner = self.inner.read().unwrap();
        inner.tasks.get(id).cloned()
    }

    /// List all tasks in a specific slot.
    pub fn list_by_slot(&self, slot: &str) -> Vec<TaskInfo> {
        let inner = self.inner.read().unwrap();

        inner
            .by_slot
            .get(slot)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.tasks.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all tasks.
    pub fn list_all(&self) -> Vec<TaskInfo> {
        let inner = self.inner.read().unwrap();
        inner.tasks.values().cloned().collect()
    }

    /// List tasks matching a status filter.
    pub fn list_by_status(&self, status: TaskStatus) -> Vec<TaskInfo> {
        let inner = self.inner.read().unwrap();
        inner
            .tasks
            .values()
            .filter(|info| info.status == status)
            .cloned()
            .collect()
    }

    /// Query tasks with combined filters and pagination.
    ///
    /// Filters are applied inside a single read lock.
    /// When `slot` is specified, uses the `by_slot` index to narrow the scan.
    /// `total` in the result reflects the count *after* filtering, *before* pagination.
    pub fn query(&self, q: &TaskQuery) -> TaskPage<TaskInfo> {
        let inner = self.inner.read().unwrap();

        // Choose the iterator source based on whether slot filter is present.
        // When slot is given we use the by_slot index to avoid full scan.
        let iter: Box<dyn Iterator<Item = &TaskInfo>> = match &q.slot {
            Some(slot) => {
                let ids = inner.by_slot.get(slot.as_str());
                match ids {
                    Some(ids) => Box::new(ids.iter().filter_map(|id| inner.tasks.get(id))),
                    None => {
                        return TaskPage {
                            items: vec![],
                            total: 0,
                        };
                    }
                }
            }
            None => Box::new(inner.tasks.values()),
        };

        // Apply status filter if present.
        let iter: Box<dyn Iterator<Item = &TaskInfo>> = match &q.status {
            Some(status) => {
                let status = *status;
                Box::new(iter.filter(move |info| info.status == status))
            }
            None => iter,
        };

        // Collect refs that pass all filters — we need total count
        // and then paginate, so we must know the full filtered set size.
        // We avoid cloning here by collecting references first.
        let filtered: Vec<&TaskInfo> = iter.collect();
        let total = filtered.len();

        let items = filtered
            .into_iter()
            .skip(q.offset)
            .take(q.limit)
            .cloned()
            .collect();

        TaskPage { items, total }
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_get_task() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");
        let slot = "demo-slot".to_string();

        state.add_task(id.clone(), slot.clone());

        let info = state.get(&id).expect("task should exist");
        assert_eq!(info.id, id);
        assert_eq!(info.slot, slot);
        assert_eq!(info.status, TaskStatus::Pending);
        assert_eq!(info.attempt, 0);
    }

    #[test]
    fn update_status_changes_task_state() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), "slot".to_string());
        state.update_status(&id, TaskStatus::Running, None);

        let info = state.get(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Running);
        assert!(info.error.is_none());
    }

    #[test]
    fn update_status_with_error() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), "slot".to_string());
        state.update_status(&id, TaskStatus::Failed, Some("timeout".to_string()));

        let info = state.get(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Failed);
        assert_eq!(info.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn increment_attempt_updates_counter() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), "slot".to_string());
        state.increment_attempt(&id);
        state.increment_attempt(&id);

        let info = state.get(&id).unwrap();
        assert_eq!(info.attempt, 2);
    }

    #[test]
    fn remove_task_deletes_from_state() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), "slot".to_string());
        assert!(state.get(&id).is_some());

        state.remove_task(&id);
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn list_by_slot_returns_correct_tasks() {
        let state = TaskState::new();

        state.add_task(TaskId::from("task-1"), "slot-a".to_string());
        state.add_task(TaskId::from("task-2"), "slot-a".to_string());
        state.add_task(TaskId::from("task-3"), "slot-b".to_string());

        let slot_a_tasks = state.list_by_slot("slot-a");
        assert_eq!(slot_a_tasks.len(), 2);

        let slot_b_tasks = state.list_by_slot("slot-b");
        assert_eq!(slot_b_tasks.len(), 1);
    }

    #[test]
    fn list_by_status_filters_correctly() {
        let state = TaskState::new();
        let id1 = TaskId::from("task-1");
        let id2 = TaskId::from("task-2");

        state.add_task(id1.clone(), "slot".to_string());
        state.add_task(id2.clone(), "slot".to_string());
        state.update_status(&id1, TaskStatus::Running, None);

        let running_tasks = state.list_by_status(TaskStatus::Running);
        assert_eq!(running_tasks.len(), 1);
        assert_eq!(running_tasks[0].id, id1);

        let pending_tasks = state.list_by_status(TaskStatus::Pending);
        assert_eq!(pending_tasks.len(), 1);
        assert_eq!(pending_tasks[0].id, id2);
    }

    #[test]
    fn list_all_returns_all_tasks() {
        let state = TaskState::new();

        state.add_task(TaskId::from("task-1"), "slot-a".to_string());
        state.add_task(TaskId::from("task-2"), "slot-b".to_string());
        state.add_task(TaskId::from("task-3"), "slot-c".to_string());

        let all_tasks = state.list_all();
        assert_eq!(all_tasks.len(), 3);
    }

    fn setup_query_state() -> TaskState {
        let state = TaskState::new();
        // slot-a: 3 tasks (2 running, 1 pending)
        state.add_task(TaskId::from("a1"), "slot-a".to_string());
        state.add_task(TaskId::from("a2"), "slot-a".to_string());
        state.add_task(TaskId::from("a3"), "slot-a".to_string());
        state.update_status(&TaskId::from("a1"), TaskStatus::Running, None);
        state.update_status(&TaskId::from("a2"), TaskStatus::Running, None);

        // slot-b: 2 tasks (1 failed, 1 pending)
        state.add_task(TaskId::from("b1"), "slot-b".to_string());
        state.add_task(TaskId::from("b2"), "slot-b".to_string());
        state.update_status(&TaskId::from("b1"), TaskStatus::Failed, Some("err".into()));

        state
    }

    #[test]
    fn query_no_filters_returns_all() {
        let state = setup_query_state();
        let page = state.query(&TaskQuery::new().with_limit(100));
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 5);
    }

    #[test]
    fn query_by_slot_only() {
        let state = setup_query_state();
        let page = state.query(&TaskQuery::new().with_slot("slot-a"));
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 3);
    }

    #[test]
    fn query_by_status_only() {
        let state = setup_query_state();
        let page = state.query(&TaskQuery::new().with_status(TaskStatus::Running));
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn query_by_slot_and_status() {
        let state = setup_query_state();
        let page = state.query(
            &TaskQuery::new()
                .with_slot("slot-a")
                .with_status(TaskStatus::Running),
        );
        assert_eq!(page.total, 2);
        assert!(page.items.iter().all(|t| t.status == TaskStatus::Running));
    }

    #[test]
    fn query_by_slot_and_status_no_match() {
        let state = setup_query_state();
        let page = state.query(
            &TaskQuery::new()
                .with_slot("slot-b")
                .with_status(TaskStatus::Running),
        );
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    #[test]
    fn query_unknown_slot_returns_empty() {
        let state = setup_query_state();
        let page = state.query(&TaskQuery::new().with_slot("nonexistent"));
        assert_eq!(page.total, 0);
        assert!(page.items.is_empty());
    }

    #[test]
    fn query_pagination_offset_and_limit() {
        let state = setup_query_state();
        // 5 total tasks, offset 2 limit 2 => items 2, total 5
        let page = state.query(&TaskQuery::new().with_limit(2).with_offset(2));
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn query_offset_beyond_total() {
        let state = setup_query_state();
        let page = state.query(&TaskQuery::new().with_offset(100));
        assert_eq!(page.total, 5);
        assert!(page.items.is_empty());
    }

    #[test]
    fn query_limit_larger_than_remaining() {
        let state = setup_query_state();
        // offset 3, limit 100 => only 2 remaining
        let page = state.query(&TaskQuery::new().with_offset(3).with_limit(100));
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn query_slot_with_pagination() {
        let state = setup_query_state();
        // slot-a has 3 tasks, offset 1 limit 1 => 1 item, total 3
        let page = state.query(
            &TaskQuery::new()
                .with_slot("slot-a")
                .with_offset(1)
                .with_limit(1),
        );
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 1);
    }
}
