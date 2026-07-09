//! In-memory task state.
//!
//! [`TaskState`] stores tasks and execution runs in `Arc<RwLock<_>>`.
//! It is updated from taskvisor events and cleaned by the periodic sweep task.

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

use solti_model::{
    Slot, Task, TaskId, TaskKind, TaskPage, TaskPhase, TaskQuery, TaskRun, TaskSpec,
};

use crate::error::CoreError;

/// Shared in-memory task state.
///
/// `TaskState` is usually obtained from [`SupervisorApi::state`](crate::SupervisorApi::state).
/// Outside this crate it is a read handle: the supervisor owns the writes.
///
/// ## Example
///
/// ```
/// use solti_core::TaskState;
/// use solti_model::TaskQuery;
///
/// let state = TaskState::new();
/// assert!(state.list_all().is_empty());
/// assert_eq!(state.query(&TaskQuery::new()).total, 0);
/// ```
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
    /// taskvisor run identity (raw) -> task entry.
    /// The canonical correlation key for incoming events: labels are reusable, identities are not.
    by_tv: HashMap<u64, TaskId>,
    /// Task entry -> its current taskvisor run identity.
    tv_of: HashMap<TaskId, taskvisor::TaskId>,
    /// Per-task run-history cap (oldest finished runs evicted past this).
    max_runs_per_task: usize,
}

impl TaskState {
    /// Create empty task state with the default per-task run-history cap.
    ///
    /// Most applications use [`SupervisorApi::state`](crate::SupervisorApi::state) instead of constructing this directly.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.list_all().is_empty());
    /// ```
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TaskStateInner {
                by_slot: HashMap::new(),
                tasks: HashMap::new(),
                runs: HashMap::new(),
                by_tv: HashMap::new(),
                tv_of: HashMap::new(),
                max_runs_per_task: config::DEFAULT_MAX_RUNS_PER_TASK,
            })),
        }
    }

    /// Override the per-task run-history cap (see [`StateConfig::max_runs_per_task`]).
    ///
    /// Intended to be called once at wiring time, before any events arrive.
    pub(crate) fn set_max_runs_per_task(&self, max: usize) {
        self.inner.write().max_runs_per_task = max;
    }

    /// Register a task entry unconditionally.
    ///
    /// Production registration goes through [`reserve`](Self::reserve) at submit time;
    /// this unconditional insert exists only for in-crate tests and the `test-util` fixtures.
    /// It is compiled only under those configurations.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn add_task(&self, id: TaskId, spec: TaskSpec) {
        let mut inner = self.inner.write();

        let slot = spec.slot().clone();
        let task = Task::new(id.clone(), spec);

        let ids = inner.by_slot.entry(slot).or_default();
        if !ids.contains(&id) {
            ids.push(id.clone());
        }
        inner.tasks.insert(id, task);
    }

    /// Atomically reserve a task name with a provisional entry.
    ///
    /// Returns [`CoreError::AlreadyExists`] if a **non-terminal** entry already holds this name (the previous incarnation is still active).
    /// Otherwise, it inserts the provisional entry: idempotent on the slot index, exactly like [`add_task`](Self::add_task) and returns `Ok(())`.
    pub(crate) fn reserve(&self, id: TaskId, spec: TaskSpec) -> Result<(), CoreError> {
        let mut inner = self.inner.write();

        if let Some(existing) = inner.tasks.get(&id)
            && !existing.status().phase.is_terminal()
        {
            return Err(CoreError::AlreadyExists(format!(
                "task '{id}' is already active (phase {})",
                existing.status().phase
            )));
        }
        Self::unbind_locked(&mut inner, &id);

        let slot = spec.slot().clone();
        let task = Task::new(id.clone(), spec);
        let ids = inner.by_slot.entry(slot).or_default();
        if !ids.contains(&id) {
            ids.push(id.clone());
        }
        inner.tasks.insert(id, task);
        Ok(())
    }

    /// Bind a task entry to its current taskvisor run identity.
    ///
    /// Called at submission time (the id is pre-minted by `submit()`).
    /// Rebinding the same entry to a new identity drops the previous binding.
    /// Late events from the previous incarnation no longer resolve to this entry.
    pub(crate) fn bind_tv(&self, id: &TaskId, tv: taskvisor::TaskId) {
        let mut inner = self.inner.write();
        if let Some(old) = inner.tv_of.insert(id.clone(), tv) {
            inner.by_tv.remove(&old.get());
        }
        inner.by_tv.insert(tv.get(), id.clone());
    }

    /// Resolve a taskvisor run identity to its task entry (if currently bound).
    pub(crate) fn resolve_tv(&self, tv: u64) -> Option<TaskId> {
        self.inner.read().by_tv.get(&tv).cloned()
    }

    /// The taskvisor run identity currently bound to a task entry (if any).
    pub(crate) fn tv_for(&self, id: &TaskId) -> Option<taskvisor::TaskId> {
        self.inner.read().tv_of.get(id).copied()
    }

    fn unbind_locked(inner: &mut TaskStateInner, id: &TaskId) {
        if let Some(tv) = inner.tv_of.remove(id) {
            inner.by_tv.remove(&tv.get());
        }
    }

    /// Release the taskvisor identity binding for a task entry, if any.
    ///
    /// The sweep deliberately spares any entry that is still **bound**: a periodic task sits in a terminal phase between runs while its actor is alive (see [`sweep`](Self::sweep)).
    /// Terminal finalizations that do *not* flow through `TaskRemoved` (admission rejections, lag-dropped terminals reconstructed by the completion path)
    /// must therefore call this to drop the binding. Otherwise the entry is mistaken for a live periodic task and is never reaped.
    pub(crate) fn unbind(&self, id: &TaskId) {
        let mut inner = self.inner.write();
        Self::unbind_locked(&mut inner, id);
    }

    /// Unregister a task from state (called on `TaskRemoved` event).
    ///
    /// Removes the task entry, its slot index, and its identity binding, but **preserves run history** (cleaned later by sweep).
    /// Compare with [`delete_task`](Self::delete_task) which removes both.
    pub(crate) fn unregister_task(&self, id: &TaskId) {
        let mut inner = self.inner.write();

        Self::unbind_locked(&mut inner, id);
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
    pub(crate) fn delete_task(&self, id: &TaskId) -> bool {
        let mut inner = self.inner.write();
        inner.runs.remove(id);

        Self::unbind_locked(&mut inner, id);
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
    pub(crate) fn transition_starting(&self, id: &TaskId) -> Option<u32> {
        let mut inner = self.inner.write();

        let attempt = if let Some(task) = inner.tasks.get_mut(id) {
            task.transition_starting();
            task.status().attempt
        } else {
            return None;
        };

        let max_runs = inner.max_runs_per_task;
        let runs = inner.runs.entry(id.clone()).or_default();
        for run in runs.iter_mut().filter(|r| r.is_active()) {
            run.finish(
                TaskPhase::Failed,
                Some("run outcome not observed (a later attempt started first)".to_string()),
                None,
            );
        }
        runs.push_back(TaskRun::starting(attempt));

        while runs.len() > max_runs && runs.front().is_some_and(|r| !r.is_active()) {
            runs.pop_front();
        }

        Some(attempt)
    }

    /// Atomically transition a task to a terminal phase and close the active run.
    pub(crate) fn transition_finished(
        &self,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        let mut inner = self.inner.write();
        Self::transition_finished_locked(&mut inner, id, phase, error, exit_code)
    }

    fn transition_finished_locked(
        inner: &mut TaskStateInner,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
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

    /// Atomically finalize the entry bound to taskvisor run identity `tv_raw`.
    ///
    /// All checks and mutations happen under one write lock:
    ///
    /// 1. The binding must still be bidirectionally current: `by_tv[tv_raw]` resolves to an entry whose `tv_of` points back at `tv_raw`.
    ///    A stale completion waiter can therefore never touch a newer incarnation that already re-reserved the name (see [`reserve`](Self::reserve)).
    /// 2. The binding is released unconditionally: the waiter fires only once the actor has fully terminated (see [`unbind`](Self::unbind)).
    /// 3. The phase transition is applied unless the entry is already terminal - the completion path must never demote an event-derived phase.
    ///    `force` overrides that guard for rejected submissions, which must reach a terminal phase even if the event path already touched the
    ///    entry (`Task::transition_finished` stays sticky-terminal).
    ///
    /// Returns the bound entry's id so the caller can evict per-task resources (e.g. output channels) outside the lock; `None` for a stale binding.
    pub(crate) fn finalize_if_bound(
        &self,
        tv_raw: u64,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        force: bool,
    ) -> Option<TaskId> {
        let mut inner = self.inner.write();

        let id = inner.by_tv.get(&tv_raw)?.clone();
        if inner.tv_of.get(&id).map(|tv| tv.get()) != Some(tv_raw) {
            return None;
        }
        Self::unbind_locked(&mut inner, &id);

        let already_terminal = inner
            .tasks
            .get(&id)
            .is_none_or(|t| t.status().phase.is_terminal());
        if force || !already_terminal {
            Self::transition_finished_locked(&mut inner, &id, phase, error, exit_code);
        }
        Some(id)
    }

    /// List all retained runs for a task, oldest first.
    ///
    /// Returns an empty list when the task is unknown or its run history has
    /// already been swept.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::TaskId;
    ///
    /// let state = TaskState::new();
    /// let runs = state.list_runs(&TaskId::from("task-1"));
    /// assert!(runs.is_empty());
    /// ```
    pub fn list_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        let inner = self.inner.read();
        inner
            .runs
            .get(id)
            .map(|runs| runs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return one task by id.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::TaskId;
    ///
    /// let state = TaskState::new();
    /// assert!(state.get(&TaskId::from("task-1")).is_none());
    /// ```
    pub fn get(&self, id: &TaskId) -> Option<Task> {
        let inner = self.inner.read();
        inner.tasks.get(id).cloned()
    }

    /// Return `true` if a task entry currently exists for `id`.
    ///
    /// This is cheaper than [`get`](Self::get) because it does not clone the
    /// task.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::TaskId;
    ///
    /// let state = TaskState::new();
    /// assert!(!state.contains_task(&TaskId::from("task-1")));
    /// ```
    pub fn contains_task(&self, id: &TaskId) -> bool {
        self.inner.read().tasks.contains_key(id)
    }

    /// List tasks in a specific slot.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.list_by_slot("workers").is_empty());
    /// ```
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

    /// List all visible tasks.
    ///
    /// Solti internal maintenance tasks are excluded.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.list_all().is_empty());
    /// ```
    pub fn list_all(&self) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| !is_internal_slot(task.slot().as_str()))
            .cloned()
            .collect()
    }

    /// List visible tasks that match one phase.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::TaskPhase;
    ///
    /// let state = TaskState::new();
    /// assert!(state.list_by_status(TaskPhase::Running).is_empty());
    /// ```
    pub fn list_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| !is_internal_slot(task.slot().as_str()) && task.status().phase == phase)
            .cloned()
            .collect()
    }

    /// Count visible tasks per phase.
    ///
    /// This makes one read-lock pass and does not clone full [`Task`] values.
    /// It is built for metrics collectors. Phases with no tasks are absent from
    /// the map.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.count_by_phase().is_empty());
    /// ```
    pub fn count_by_phase(&self) -> HashMap<TaskPhase, usize> {
        let inner = self.inner.read();
        let mut counts: HashMap<TaskPhase, usize> = HashMap::new();
        for task in inner
            .tasks
            .values()
            .filter(|task| !is_internal_slot(task.slot().as_str()))
        {
            *counts.entry(task.status().phase).or_insert(0) += 1;
        }
        counts
    }

    /// Run a sweep pass.
    ///
    /// Two passes under a single write lock:
    /// 1. Remove finished runs older than `run_ttl`.
    /// 2. Remove terminal tasks that have no remaining runs and whose `updated_at` is older than `task_ttl`.
    ///
    /// Returns `(runs_removed, tasks_removed)` for observability.
    pub(crate) fn sweep(&self, config: &StateConfig) -> (usize, usize) {
        let mut inner = self.inner.write();
        let now = SystemTime::now();
        let mut runs_removed = 0usize;
        let mut tasks_removed = 0usize;

        let bound: std::collections::HashSet<TaskId> = inner.tv_of.keys().cloned().collect();
        for (id, runs) in inner.runs.iter_mut() {
            let before = runs.len();
            let task_bound = bound.contains(id);
            runs.retain(|run| match run.finished_at {
                Some(finished) => now
                    .duration_since(finished)
                    .map(|age| age < config.run_ttl)
                    .unwrap_or(true),
                None => {
                    task_bound
                        || now
                            .duration_since(run.started_at)
                            .map(|age| age < config.run_ttl)
                            .unwrap_or(true)
                }
            });
            runs_removed += before - runs.len();
        }
        inner.runs.retain(|_, runs| !runs.is_empty());

        let expired_tasks: Vec<TaskId> = inner
            .tasks
            .iter()
            .filter(|(id, task)| {
                !inner.tv_of.contains_key(*id)
                    && task.status().phase.is_terminal()
                    && inner.runs.get(*id).is_none_or(|runs| runs.is_empty())
                    && now
                        .duration_since(task.metadata().updated_at)
                        .map(|age| age >= config.task_ttl)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired_tasks {
            Self::unbind_locked(&mut inner, id);
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

    /// Query visible tasks with combined filters and pagination.
    ///
    /// Filters are applied inside a single read lock.
    /// When `slot` is specified, uses the `by_slot` index to narrow the scan.
    /// `total` in the result reflects the count *after* filtering, *before* pagination.
    ///
    /// This is the API-facing listing: [`TaskKind::Embedded`] entries (in-process
    /// machinery such as discovery sync or metric servers) are excluded before
    /// `total` is computed, so `total` matches the visible API result.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::{TaskPhase, TaskQuery};
    ///
    /// let state = TaskState::new();
    /// let query = TaskQuery::new().with_status(TaskPhase::Running).with_limit(10);
    /// let page = state.query(&query);
    ///
    /// assert_eq!(page.total, 0);
    /// assert!(page.items.is_empty());
    /// ```
    pub fn query(&self, q: &TaskQuery) -> TaskPage<Task> {
        let inner = self.inner.read();

        let iter: Box<dyn Iterator<Item = &Task>> = match q.slot() {
            Some(slot) => {
                let ids = inner.by_slot.get(slot.as_str());
                match ids {
                    Some(ids) => Box::new(
                        ids.iter()
                            .filter_map(|id| inner.tasks.get(id))
                            .filter(|task| is_query_visible(task)),
                    ),
                    None => {
                        return TaskPage {
                            items: vec![],
                            total: 0,
                        };
                    }
                }
            }
            None => Box::new(inner.tasks.values().filter(|task| is_query_visible(task))),
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

/// Test-only fixtures for populating state from outside the crate.
#[cfg(feature = "test-util")]
impl TaskState {
    /// Seed a task entry directly (test fixtures only).
    pub fn seed_task(&self, id: TaskId, spec: TaskSpec) {
        self.add_task(id, spec);
    }

    /// Transition a seeded task to `Running` (test fixtures only).
    pub fn seed_starting(&self, id: &TaskId) {
        self.transition_starting(id);
    }

    /// Transition a seeded task to a terminal phase (test fixtures only).
    pub fn seed_finished(
        &self,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) {
        self.transition_finished(id, phase, error, exit_code);
    }
}

/// `true` for slots owned by solti-core's own embedded machinery.
fn is_internal_slot(slot: &str) -> bool {
    slot == sweep::SWEEP_SLOT
}

/// `true` if a task may surface through the paginated query API.
///
/// Internal slots and [`TaskKind::Embedded`] entries are host-side machinery:
/// they must be cut *before* `total` is computed, or pagination arithmetic on
/// the API boundary stops adding up (`solti-api` keeps its own filter purely
/// as a safety net).
fn is_query_visible(task: &Task) -> bool {
    !is_internal_slot(task.slot().as_str()) && !matches!(task.spec().kind(), TaskKind::Embedded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv};

    fn subprocess_kind() -> TaskKind {
        TaskKind::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: vec![],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        ))
    }

    fn default_spec_with_slot(slot: &str) -> TaskSpec {
        TaskSpec::builder(slot, subprocess_kind(), 5_000_u64)
            .build()
            .expect("valid spec")
    }

    fn embedded_spec_with_slot(slot: &str) -> TaskSpec {
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
    fn add_task_twice_with_same_id_does_not_duplicate_in_slot() {
        let state = TaskState::new();
        let id = TaskId::from("dup");

        state.add_task(id.clone(), default_spec_with_slot("slot-x"));
        state.add_task(id.clone(), default_spec_with_slot("slot-x"));

        assert_eq!(
            state.list_by_slot("slot-x").len(),
            1,
            "re-adding the same id must not duplicate it in the slot index"
        );
    }

    #[test]
    fn reserve_rejects_a_second_active_name() {
        let state = TaskState::new();
        let id = TaskId::from("dup");

        assert!(state.reserve(id.clone(), default_spec()).is_ok());
        let err = state
            .reserve(id.clone(), default_spec())
            .expect_err("second reservation of an active name must fail");
        assert!(matches!(err, CoreError::AlreadyExists(_)));
        assert_eq!(state.list_by_slot("slot").len(), 1);
    }

    #[test]
    fn reserve_allows_reusing_a_terminal_name() {
        let state = TaskState::new();
        let id = TaskId::from("reuse");

        state.reserve(id.clone(), default_spec()).unwrap();
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        assert!(
            state.reserve(id.clone(), default_spec()).is_ok(),
            "a terminal name must be reusable"
        );
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
    fn internal_sweep_slot_is_hidden_from_general_listings() {
        let state = TaskState::new();
        state.add_task(
            TaskId::from("user-task"),
            default_spec_with_slot("user-slot"),
        );
        state.add_task(
            TaskId::from(super::sweep::SWEEP_SLOT),
            embedded_spec_with_slot(super::sweep::SWEEP_SLOT),
        );

        assert_eq!(state.list_all().len(), 1);
        assert_eq!(state.list_all()[0].slot(), "user-slot");
        assert_eq!(
            state.list_by_status(TaskPhase::Pending).len(),
            1,
            "by-status listing also omits the internal sweep task"
        );
        let page = state.query(&TaskQuery::new().with_limit(100));
        assert_eq!(
            page.total, 1,
            "slot-less query omits the internal sweep task"
        );
        assert_eq!(page.items.len(), 1);

        let page = state.query(&TaskQuery::new().with_slot(super::sweep::SWEEP_SLOT));
        assert_eq!(
            page.total, 0,
            "the slot-indexed query branch hides embedded tasks too"
        );
        assert!(page.items.is_empty());
        assert_eq!(
            state.list_by_slot(super::sweep::SWEEP_SLOT).len(),
            1,
            "the raw in-crate slot listing still sees the entry"
        );
    }

    #[test]
    fn query_excludes_embedded_tasks_before_pagination() {
        let state = TaskState::new();
        state.add_task(TaskId::from("user-a"), default_spec_with_slot("mixed"));
        state.add_task(TaskId::from("user-b"), default_spec_with_slot("mixed"));
        state.add_task(TaskId::from("user-c"), default_spec_with_slot("mixed"));
        state.add_task(TaskId::from("user-d"), default_spec_with_slot("mixed"));
        // Embedded entries sort *before* "user-*": if they were cut only after
        // pagination, the first pages would come up short of `total`.
        for name in [
            "solti-discover-sync",
            "solti-logger-tz-sync",
            "solti-metrics-server",
        ] {
            state.add_task(TaskId::from(name), embedded_spec_with_slot("mixed"));
        }

        let mut seen = 0usize;
        for offset in [0usize, 2, 4] {
            let page = state.query(&TaskQuery::new().with_offset(offset).with_limit(2));
            assert_eq!(page.total, 4, "total counts only routable tasks");
            assert!(
                page.items
                    .iter()
                    .all(|t| !matches!(t.spec().kind(), TaskKind::Embedded)),
                "no page may leak an embedded task"
            );
            seen += page.items.len();
        }
        assert_eq!(seen, 4, "pages must add up to exactly total");

        // The slot-indexed branch (?slot=) applies the same visibility rule.
        let page = state.query(&TaskQuery::new().with_slot("mixed").with_limit(100));
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 4);
        assert!(
            page.items
                .iter()
                .all(|t| !matches!(t.spec().kind(), TaskKind::Embedded))
        );
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

        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, Some("err".into()), None);

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
        state.add_task(TaskId::from("a1"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("a2"), default_spec_with_slot("slot-a"));
        state.add_task(TaskId::from("a3"), default_spec_with_slot("slot-a"));
        state.transition_starting(&TaskId::from("a1"));
        state.transition_starting(&TaskId::from("a2"));

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

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::from_secs(3600),
            sweep_interval: std::time::Duration::from_secs(60),
            ..StateConfig::default()
        };

        let (runs_removed, tasks_removed) = state.sweep(&config);
        assert_eq!(runs_removed, 1);
        assert_eq!(tasks_removed, 0);
        assert!(state.list_runs(&id).is_empty());
    }

    #[test]
    fn sweep_keeps_terminal_tasks_with_live_binding() {
        let state = TaskState::new();
        let id = TaskId::from("periodic");

        state.add_task(id.clone(), default_spec());
        state.bind_tv(&id, taskvisor::TaskId::for_tests());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            sweep_interval: std::time::Duration::from_secs(60),
            ..StateConfig::default()
        };

        let (_, tasks_removed) = state.sweep(&config);
        assert_eq!(
            tasks_removed, 0,
            "a task whose actor is still alive (bound) must survive the sweep"
        );
        assert!(state.get(&id).is_some());
    }

    #[test]
    fn unbind_lets_sweep_reap_a_terminal_bound_entry() {
        let state = TaskState::new();
        let id = TaskId::from("leaked");

        state.add_task(id.clone(), default_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, Some("rejected".into()), None);

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            ..StateConfig::default()
        };

        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 0, "a bound terminal entry must NOT be swept");
        assert!(state.get(&id).is_some());

        state.unbind(&id);
        let (_, removed) = state.sweep(&config);
        assert_eq!(removed, 1, "an unbound terminal entry must be reaped");
        assert!(state.get(&id).is_none());
        assert!(state.resolve_tv(tv.get()).is_none());
    }

    #[test]
    fn sweep_removes_terminal_tasks_without_runs() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Succeeded, None, None);

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            sweep_interval: std::time::Duration::from_secs(60),
            ..StateConfig::default()
        };

        let (_, tasks_removed) = state.sweep(&config);
        assert_eq!(tasks_removed, 1);
        assert!(state.get(&id).is_none());
    }

    #[test]
    fn sweep_keeps_active_runs_of_a_live_task() {
        let state = TaskState::new();
        let id = TaskId::from("task-1");

        state.add_task(id.clone(), default_spec());
        state.bind_tv(&id, taskvisor::TaskId::for_tests());
        state.transition_starting(&id);

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::ZERO,
            ..StateConfig::default()
        };

        let (runs_removed, _) = state.sweep(&config);
        assert_eq!(runs_removed, 0);
        assert_eq!(state.list_runs(&id).len(), 1);
    }

    #[test]
    fn sweep_ages_out_active_run_on_an_unbound_task() {
        let state = TaskState::new();
        let id = TaskId::from("orphan-run");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id); // active run, task never bound

        let config = StateConfig {
            run_ttl: std::time::Duration::ZERO,
            task_ttl: std::time::Duration::from_secs(3600),
            ..StateConfig::default()
        };

        let (runs_removed, _) = state.sweep(&config);
        assert_eq!(
            runs_removed, 1,
            "an active run on an unbound (dead) task is aged out"
        );
        assert!(state.list_runs(&id).is_empty());
    }

    #[test]
    fn run_history_is_capped_per_task() {
        let state = TaskState::new();
        state.set_max_runs_per_task(3);
        let id = TaskId::from("chatty");
        state.add_task(id.clone(), default_spec());

        for _ in 0..5 {
            state.transition_starting(&id);
            state.transition_finished(&id, TaskPhase::Failed, None, None);
        }

        let runs = state.list_runs(&id);
        assert_eq!(
            runs.len(),
            3,
            "run history must be capped at max_runs_per_task"
        );
        assert_eq!(
            runs.first().unwrap().attempt,
            3,
            "oldest retained is attempt 3"
        );
        assert_eq!(
            runs.last().unwrap().attempt,
            5,
            "newest retained is attempt 5"
        );
    }

    #[test]
    fn run_history_cap_never_evicts_the_active_tail() {
        let state = TaskState::new();
        state.set_max_runs_per_task(2);
        let id = TaskId::from("tail");
        state.add_task(id.clone(), default_spec());

        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, None, None);
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, None, None);
        state.transition_starting(&id);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 2);
        assert!(
            runs.last().unwrap().is_active(),
            "the in-flight run is preserved"
        );
        assert_eq!(runs.last().unwrap().attempt, 3);
    }

    #[test]
    fn new_start_closes_a_prior_orphaned_active_run() {
        let state = TaskState::new();
        let id = TaskId::from("retry");

        state.add_task(id.clone(), default_spec());
        state.transition_starting(&id);
        state.transition_starting(&id);

        let runs = state.list_runs(&id);
        assert_eq!(runs.len(), 2);
        assert!(
            !runs[0].is_active(),
            "the prior attempt's orphaned run must be closed when a new attempt starts"
        );
        assert!(runs[1].is_active(), "the newest attempt's run stays active");
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
            ..StateConfig::default()
        };

        let (_, tasks_removed) = state.sweep(&config);
        assert_eq!(tasks_removed, 0);
        assert!(state.get(&id).is_some());
    }

    #[test]
    fn query_slot_with_pagination() {
        let state = setup_query_state();
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

        assert_eq!(state.transition_starting(&id), Some(1));
        state.transition_finished(&id, TaskPhase::Failed, Some("err".into()), None);

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

    #[test]
    fn finalize_if_bound_finalizes_and_unbinds_a_current_binding() {
        let state = TaskState::new();
        let id = TaskId::from("bound");
        state.add_task(id.clone(), default_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);

        let finalized = state.finalize_if_bound(tv.get(), TaskPhase::Succeeded, None, None, false);

        assert_eq!(finalized, Some(id.clone()));
        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Succeeded);
        assert!(state.tv_for(&id).is_none(), "binding must be released");
        assert!(state.resolve_tv(tv.get()).is_none());
    }

    #[test]
    fn finalize_if_bound_ignores_a_stale_binding_after_rebind() {
        let state = TaskState::new();
        let id = TaskId::from("rebound");
        state.add_task(id.clone(), default_spec());
        let old = taskvisor::TaskId::for_tests();
        let new = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, old);
        state.bind_tv(&id, new);
        state.transition_starting(&id);

        let finalized = state.finalize_if_bound(
            old.get(),
            TaskPhase::Failed,
            Some("stale".into()),
            None,
            false,
        );

        assert_eq!(finalized, None, "a stale binding must be a no-op");
        assert_eq!(state.get(&id).unwrap().status().phase, TaskPhase::Running);
        assert_eq!(
            state.tv_for(&id).map(|tv| tv.get()),
            Some(new.get()),
            "the current binding must survive"
        );
    }

    #[test]
    fn finalize_if_bound_keeps_a_terminal_phase_but_still_unbinds() {
        let state = TaskState::new();
        let id = TaskId::from("done");
        state.add_task(id.clone(), default_spec());
        let tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, tv);
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, Some("boom".into()), Some(1));

        let finalized = state.finalize_if_bound(tv.get(), TaskPhase::Succeeded, None, None, false);

        assert_eq!(finalized, Some(id.clone()), "the binding was current");
        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Failed,
            "an already-terminal entry keeps its event-derived phase"
        );
        assert!(
            state.tv_for(&id).is_none(),
            "the binding is released even when the phase is kept"
        );
    }

    #[test]
    fn reserve_severs_the_previous_incarnations_binding() {
        let state = TaskState::new();
        let id = TaskId::from("respawn");
        state.reserve(id.clone(), default_spec()).unwrap();
        let old_tv = taskvisor::TaskId::for_tests();
        state.bind_tv(&id, old_tv);
        state.transition_starting(&id);
        state.transition_finished(&id, TaskPhase::Failed, None, None);

        // The new incarnation reserved the name; its bind_tv has not happened yet.
        state.reserve(id.clone(), default_spec()).unwrap();

        assert!(
            state.resolve_tv(old_tv.get()).is_none(),
            "reserve must sever the previous incarnation's binding"
        );
        assert_eq!(
            state.finalize_if_bound(old_tv.get(), TaskPhase::Succeeded, None, None, false),
            None,
            "a stale waiter must not touch the fresh entry"
        );
        assert_eq!(
            state.get(&id).unwrap().status().phase,
            TaskPhase::Pending,
            "the new incarnation stays untouched"
        );
    }

    #[test]
    fn count_by_phase_counts_without_clones_or_internal_tasks() {
        let state = TaskState::new();
        state.add_task(TaskId::from("p1"), default_spec_with_slot("s"));
        state.add_task(TaskId::from("r1"), default_spec_with_slot("s"));
        state.add_task(TaskId::from("r2"), default_spec_with_slot("s"));
        state.add_task(TaskId::from("f1"), default_spec_with_slot("s"));
        state.transition_starting(&TaskId::from("r1"));
        state.transition_starting(&TaskId::from("r2"));
        state.transition_starting(&TaskId::from("f1"));
        state.transition_finished(&TaskId::from("f1"), TaskPhase::Failed, None, None);
        state.add_task(
            TaskId::from(super::sweep::SWEEP_SLOT),
            embedded_spec_with_slot(super::sweep::SWEEP_SLOT),
        );

        let counts = state.count_by_phase();
        assert_eq!(
            counts.get(&TaskPhase::Pending).copied(),
            Some(1),
            "the internal sweep task must not be counted"
        );
        assert_eq!(counts.get(&TaskPhase::Running).copied(), Some(2));
        assert_eq!(counts.get(&TaskPhase::Failed).copied(), Some(1));
        assert_eq!(
            counts.get(&TaskPhase::Succeeded),
            None,
            "empty phases are absent from the map"
        );
        assert_eq!(counts.values().sum::<usize>(), 4);
    }
}
