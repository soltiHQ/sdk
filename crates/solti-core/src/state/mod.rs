//! # In-memory task state.
//!
//! [`TaskState`] stores tasks and execution runs in `Arc<RwLock<_>>`.
//! Updated from taskvisor events via [`StateSubscriber`], cleaned by the sweep task produced by [`state_sweep`].

mod sweep;
pub use sweep::state_sweep;

mod subscriber;
pub use subscriber::StateSubscriber;

mod config;
pub use config::StateConfig;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::SystemTime,
};

use parking_lot::RwLock;
use tracing::debug;

use solti_model::{Slot, Task, TaskId, TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec};

/// In-memory task state storage.
///
/// ## Also
///
/// - [`StateConfig`] TTL settings consumed by [`sweep`](Self::sweep).
/// - [`StateSubscriber`] wires taskvisor events into mutations.
/// - [`state_sweep`] builds an embedded periodic sweep task.
#[derive(Clone)]
pub struct TaskState {
    inner: Arc<RwLock<TaskStateInner>>,
}

struct TaskStateInner {
    /// Tasks indexed by TaskId.
    tasks: HashMap<TaskId, Task>,
    /// Index: slot -> list of task IDs in that slot.
    by_slot: HashMap<Slot, Vec<TaskId>>,
    /// Execution history: task_id -> ordered list of runs (oldest first).
    runs: HashMap<TaskId, VecDeque<TaskRun>>,
}

impl TaskState {
    /// Create empty task state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TaskStateInner {
                by_slot: HashMap::new(),
                tasks: HashMap::new(),
                runs: HashMap::new(),
            })),
        }
    }

    /// Register a new task (called on TaskAdded event).
    pub fn add_task(&self, id: TaskId, spec: TaskSpec) {
        let mut inner = self.inner.write();

        let slot = spec.slot().clone();
        let task = Task::new(id.clone(), spec);

        inner.by_slot.entry(slot).or_default().push(id.clone());
        inner.tasks.insert(id, task);
    }

    /// Unregister a task from state (called on `TaskRemoved` event).
    ///
    /// Removes the task entry and its slot index, but **preserves run history** (cleaned later by sweep).
    /// Compare with [`delete_task`](Self::delete_task) which removes both.
    pub fn unregister_task(&self, id: &TaskId) {
        let mut inner = self.inner.write();

        if let Some(task) = inner.tasks.remove(id)
            && let Some(ids) = inner.by_slot.get_mut(task.slot())
        {
            ids.retain(|task_id| task_id != id);
            if ids.is_empty() {
                inner.by_slot.remove(task.slot());
            }
        }
    }

    /// Delete a task **and** its run history. Returns `true` if the task existed.
    ///
    /// This is the API-driven full removal.
    /// Compare with [`unregister_task`](Self::unregister_task) which preserves runs.
    pub fn delete_task(&self, id: &TaskId) -> bool {
        let mut inner = self.inner.write();
        inner.runs.remove(id);

        if let Some(task) = inner.tasks.remove(id) {
            if let Some(ids) = inner.by_slot.get_mut(task.slot()) {
                ids.retain(|task_id| task_id != id);
                if ids.is_empty() {
                    inner.by_slot.remove(task.slot());
                }
            }
            true
        } else {
            false
        }
    }

    /// Atomically transition a task to `Running`.
    pub fn transition_starting(&self, id: &TaskId) -> Option<u32> {
        let mut inner = self.inner.write();

        let attempt = if let Some(task) = inner.tasks.get_mut(id) {
            task.transition_starting();
            task.status().attempt
        } else {
            return None;
        };

        let run = TaskRun::starting(attempt);
        inner.runs.entry(id.clone()).or_default().push_back(run);

        Some(attempt)
    }

    /// Atomically transition a task to a terminal phase and close the active run.
    pub fn transition_finished(
        &self,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        let mut inner = self.inner.write();

        let found = if let Some(task) = inner.tasks.get_mut(id) {
            match task.transition_finished(phase, error.clone(), exit_code) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(task = %id, error = %e, "ignoring illegal transition");
                    return false;
                }
            }
        } else {
            false
        };

        if let Some(runs) = inner.runs.get_mut(id)
            && let Some(run) = runs.back_mut().filter(|r| r.is_active())
        {
            run.finish(phase, error, exit_code);
        }

        found
    }

    /// List all runs for a task (oldest first).
    pub fn list_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        let inner = self.inner.read();
        inner
            .runs
            .get(id)
            .map(|runs| runs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get task by ID.
    pub fn get(&self, id: &TaskId) -> Option<Task> {
        let inner = self.inner.read();
        inner.tasks.get(id).cloned()
    }

    /// List all tasks in a specific slot.
    pub fn list_by_slot(&self, slot: &str) -> Vec<Task> {
        let inner = self.inner.read();

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
    pub fn list_all(&self) -> Vec<Task> {
        let inner = self.inner.read();
        inner.tasks.values().cloned().collect()
    }

    /// List tasks matching a phase filter.
    pub fn list_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| task.status().phase == phase)
            .cloned()
            .collect()
    }

    /// Run a sweep pass.
    ///
    /// Two passes under a single write lock:
    /// 1. Remove finished runs older than `run_ttl`.
    /// 2. Remove terminal tasks that have no remaining runs and whose `updated_at` is older than `task_ttl`.
    ///
    /// Returns `(runs_removed, tasks_removed)` for observability.
    pub fn sweep(&self, config: &StateConfig) -> (usize, usize) {
        let mut inner = self.inner.write();
        let now = SystemTime::now();
        let mut runs_removed = 0usize;
        let mut tasks_removed = 0usize;

        for runs in inner.runs.values_mut() {
            let before = runs.len();
            runs.retain(|run| {
                if let Some(finished) = run.finished_at {
                    now.duration_since(finished)
                        .map(|age| age < config.run_ttl)
                        .unwrap_or(true)
                } else {
                    true
                }
            });
            runs_removed += before - runs.len();
        }
        inner.runs.retain(|_, runs| !runs.is_empty());

        let expired_tasks: Vec<TaskId> = inner
            .tasks
            .iter()
            .filter(|(id, task)| {
                task.status().phase.is_terminal()
                    && inner.runs.get(*id).is_none_or(|runs| runs.is_empty())
                    && now
                        .duration_since(task.metadata().updated_at)
                        .map(|age| age >= config.task_ttl)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired_tasks {
            if let Some(task) = inner.tasks.remove(id) {
                if let Some(ids) = inner.by_slot.get_mut(task.slot()) {
                    ids.retain(|task_id| task_id != id);
                    if ids.is_empty() {
                        inner.by_slot.remove(task.slot());
                    }
                }
                tasks_removed += 1;
            }
        }
        if runs_removed > 0 || tasks_removed > 0 {
            debug!(runs_removed, tasks_removed, "state sweep completed");
        }

        (runs_removed, tasks_removed)
    }

    /// Query tasks with combined filters and pagination.
    ///
    /// Filters are applied inside a single read lock.
    /// When `slot` is specified, uses the `by_slot` index to narrow the scan.
    /// `total` in the result reflects the count *after* filtering, *before* pagination.
    pub fn query(&self, q: &TaskQuery) -> TaskPage<Task> {
        let inner = self.inner.read();

        let iter: Box<dyn Iterator<Item = &Task>> = match q.slot() {
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

        let iter: Box<dyn Iterator<Item = &Task>> = if q.status_filters().is_empty() {
            iter
        } else {
            Box::new(iter.filter(|task| q.matches_phase(&task.status().phase)))
        };

        let mut filtered: Vec<&Task> = iter.collect();
        filtered.sort_by(|a, b| a.metadata().id.cmp(&b.metadata().id));
        let total = filtered.len();

        let start = q.offset().min(total);
        let items = filtered[start..]
            .iter()
            .take(q.limit())
            .map(|task| (*task).clone())
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
    use solti_model::TaskKind;

    fn default_spec_with_slot(slot: &str) -> TaskSpec {
        TaskSpec::builder(slot, TaskKind::Embedded, 5_000_u64)
            .build()
            .expect("valid spec")
    }

    fn default_spec() -> TaskSpec {
        default_spec_with_slot("slot")
    }

    #[test]
    fn add_and_get_task() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec_with_slot("demo-slot"));

        let task = state.get(&id).expect("task should exist");
        assert_eq!(task.metadata().id, id);
        assert_eq!(task.slot(), "demo-slot");
        assert_eq!(task.status().phase, TaskPhase::Pending);
        assert_eq!(task.status().attempt, 0);
    }

    #[test]
    fn transition_starting_changes_phase_and_attempt() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().phase, TaskPhase::Running);
        assert!(task.status().error.is_none());
        assert_eq!(task.status().attempt, 1);
    }

    #[test]
    fn transition_finished_records_error() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, Some("timeout".to_string()), None);

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().error.as_deref(), Some("timeout"));
    }

    #[test]
    fn multiple_starts_increment_attempt() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        assert_eq!(state.transition_starting(&id), Some(1));
        state.transition_finished(&id, TaskPhase::Failed, None, None);
        assert_eq!(state.transition_starting(&id), Some(2));

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().attempt, 2);
    }

    #[test]
    fn unregister_task_removes_from_state() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        assert!(state.get(&id).is_some());

        state.unregister_task(&id);
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn list_by_slot_returns_correct_tasks() {
        let state = TaskState::new();

        state.add_task(TaskId::from("task-1"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("task-2"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("task-3"), default_spec_with_slot("slot-b"));

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

        state.add_task(id1.clone(), default_spec());
        state.add_task(id2.clone(), default_spec());
        state.transition_starting(&id1);

        let running_tasks = state.list_by_status(TaskPhase::Running);
        assert_eq!(running_tasks.len(), 1);
        assert_eq!(running_tasks[0].metadata().id, id1);

        let pending_tasks = state.list_by_status(TaskPhase::Pending);
        assert_eq!(pending_tasks.len(), 1);
        assert_eq!(pending_tasks[0].metadata().id, id2);
    }

    #[test]
    fn list_all_returns_all_tasks() {
        let state = TaskState::new();

        state.add_task(TaskId::from("task-1"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("task-2"), default_spec_with_slot("slot-b"));
        state.add_task(TaskId::from("task-3"), default_spec_with_slot("slot-c"));

        let all_tasks = state.list_all();
        assert_eq!(all_tasks.len(), 3);
    }

    #[test]
    fn transition_starting_creates_active_run() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].attempt, 1);
        assert!(runs[0].is_active());
    }

    #[test]
    fn transition_finished_closes_active_run() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].is_active());
        assert_eq!(runs[0].phase, TaskPhase::Succeeded);
    }

    #[test]
    fn multiple_runs_ordered_by_attempt() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());

        // First attempt fails
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, Some("err".into()), None);

        // Second attempt succeeds
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].attempt, 1);
        assert_eq!(runs[0].phase, TaskPhase::Failed);
        assert_eq!(runs[1].attempt, 2);
        assert_eq!(runs[1].phase, TaskPhase::Succeeded);
    }

    #[test]
    fn unregister_task_preserves_runs() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        state.unregister_task(&id);

        assert!(state.get(&id).is_none());
        // Runs survive unregister (cleaned by sweep).
        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn list_runs_empty_for_unknown_task() {
        let state = TaskState::new();
        let runs = state.list_runs(&TaskId::from("nonexistent"));
        assert!(runs.is_empty());
    }

    fn setup_query_state() -> TaskState {
        let state = TaskState::new();
        // slot-a: 3 tasks (2 running, 1 pending)
        state.add_task(TaskId::from("a1"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("a2"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("a3"), default_spec_with_slot("slot-a"));
        state.transition_starting(&TaskId::from("a1"));
        state.transition_starting(&TaskId::from("a2"));

        // slot-b: 2 tasks (1 failed, 1 pending)
        state.add_task(TaskId::from("b1"), default_spec_with_slot("slot-b"));
        state.add_task(TaskId::from("b2"), default_spec_with_slot("slot-b"));
        state.transition_starting(&TaskId::from("b1"));
        state.transition_finished(
            &TaskId::from("b1"),
            TaskPhase::Failed,
            Some("err".into()),
            None,
        );

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
        let page = state.query(&TaskQuery::new().with_status(TaskPhase::Running));
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn query_by_slot_and_status() {
        let state = setup_query_state();
        let page = state.query(
            &TaskQuery::new()
                .with_slot("slot-a")
                .with_status(TaskPhase::Running),
        );
        assert_eq!(page.total, 2);
        assert!(
            page.items
                .iter()
                .all(|t| t.status().phase == TaskPhase::Running)
        );
    }

    #[test]
    fn query_by_slot_and_status_no_match() {
        let state = setup_query_state();
        let page = state.query(
            &TaskQuery::new()
                .with_slot("slot-b")
                .with_status(TaskPhase::Running),
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
    fn sweep_removes_expired_runs() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        // With zero TTL, everything is expired.
        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::from_secs(3600),
            sweep_interval: std::time::Duration::from_secs(60),
        };

        let (runs_removed, tasks_removed) = state.sweep(&config);
        assert_eq!(runs_removed, 1);
        assert_eq!(tasks_removed, 0); // task still exists (task_ttl is long)
        assert!(state.list_runs(&id).is_empty());
    }

    #[test]
    fn sweep_removes_terminal_tasks_without_runs() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);
        // Run will also be swept since run_ttl is zero.

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            sweep_interval: std::time::Duration::from_secs(60),
        };

        let (_, tasks_removed) = state.sweep(&config);
        assert_eq!(tasks_removed, 1);
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn sweep_keeps_active_runs() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        // run is still active (not finished)

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            sweep_interval: std::time::Duration::from_secs(60),
        };

        let (runs_removed, _) = state.sweep(&config);
        assert_eq!(runs_removed, 0);
        assert_eq!(state.list_runs(&id).len(), 1);
    }

    #[test]
    fn sweep_keeps_non_terminal_tasks() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            sweep_interval: std::time::Duration::from_secs(60),
        };

        let (_, tasks_removed) = state.sweep(&config);
        assert_eq!(tasks_removed, 0);
        assert!(state.get(&id).is_some());
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

    #[test]
    fn transition_starting_atomically_updates_state() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());

        let attempt = state.transition_starting(&id);
        assert_eq!(attempt, Some(1));

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().phase, TaskPhase::Running);
        assert_eq!(task.status().attempt, 1);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].attempt, 1);
        assert!(runs[0].is_active());
    }

    #[test]
    fn transition_starting_returns_none_for_unknown_task() {
        let state = TaskState::new();
        assert_eq!(state.transition_starting(&TaskId::from("nope")), None);
    }

    #[test]
    fn transition_finished_atomically_updates_state() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);

        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), None);

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().phase, TaskPhase::Failed);
        assert_eq!(task.status().error.as_deref(), Some("boom"));

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].is_active());
        assert_eq!(runs[0].phase, TaskPhase::Failed);
    }

    #[test]
    fn transition_finished_success_no_error() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().phase, TaskPhase::Succeeded);
        assert!(task.status().error.is_none());

        let runs = state.list_runs(&id);
        assert_eq!(runs[0].phase, TaskPhase::Succeeded);
        assert!(!runs[0].is_active());
    }

    #[test]
    fn transition_starting_multiple_attempts() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());

        // Attempt 1: start → fail
        assert_eq!(state.transition_starting(&id), Some(1));
        state.transition_finished(&id, TaskPhase::Failed, Some("err".into()), None);

        // Attempt 2: start → succeed
        assert_eq!(state.transition_starting(&id), Some(2));
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let task = state.get(&id).unwrap();
        assert_eq!(task.status().attempt, 2);
        assert_eq!(task.status().phase, TaskPhase::Succeeded);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].attempt, 1);
        assert_eq!(runs[0].phase, TaskPhase::Failed);
        assert_eq!(runs[1].attempt, 2);
        assert_eq!(runs[1].phase, TaskPhase::Succeeded);
    }
}
