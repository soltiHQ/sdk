//! In-memory task state.
//!
//! [`TaskState`] stores tasks and execution runs in `Arc<RwLock<_>>`.
//! It is updated from taskvisor events and cleaned by the periodic sweep task.

mod sweep;
pub(crate) use sweep::{SWEEP_NAME, SWEEP_SLOT, state_sweep};

mod subscriber;
pub use subscriber::StateSubscriber;

mod config;
pub use config::StateConfig;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::SystemTime,
};

use parking_lot::{Mutex, MutexGuard, RwLock};
use tracing::debug;

use solti_model::{
    DesiredChange, Slot, Task, TaskId, TaskManifest, TaskPage, TaskPhase, TaskQuery, TaskRun, Uid,
    WorkloadTypeMeta,
};

use crate::error::CoreError;

/// Serializes short state/output lifecycle commits across the event, waiter,
/// and management paths.
///
/// Async controller waits stay outside this gate. It only closes the small
/// resolve-then-mutate windows where a reusable [`TaskId`] could otherwise let
/// an old incarnation touch a newer one.
#[derive(Clone, Default)]
pub(crate) struct LifecycleGate {
    inner: Arc<Mutex<()>>,
}

impl LifecycleGate {
    pub(crate) fn lock(&self) -> MutexGuard<'_, ()> {
        self.inner.lock()
    }
}

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
    /// Raw taskvisor identity -> exact resource incarnation and generation.
    by_tv: HashMap<u64, RuntimeBinding>,
    /// Resource name -> its current taskvisor binding.
    tv_of: HashMap<TaskId, RuntimeBinding>,
    /// Highest terminal attempt already projected for each live Taskvisor binding.
    /// Kept independently from user-visible run retention so duplicate runtime
    /// events remain idempotent even after their TaskRun has been evicted.
    finished_attempt_by_tv: HashMap<u64, u32>,
    /// Store-global opaque resource-version source.
    next_resource_version: u64,
    /// Internal retention clock for terminal resources.
    terminal_since: HashMap<TaskId, SystemTime>,
    /// Per-task run-history cap (oldest finished runs evicted past this).
    max_runs_per_task: usize,
}

/// Exact resource incarnation and desired generation associated with one runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResourceGeneration {
    pub(crate) name: TaskId,
    pub(crate) uid: Uid,
    pub(crate) generation: u64,
    pub(crate) workload: WorkloadTypeMeta,
}

impl ResourceGeneration {
    pub(crate) fn from_task(task: &Task) -> Self {
        Self {
            name: task.name().clone(),
            uid: task.uid().clone(),
            generation: task.metadata().generation(),
            workload: task.spec().workload().type_meta(),
        }
    }
}

/// Correlation between a Taskvisor submission and one exact Task generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeBinding {
    pub(crate) resource: ResourceGeneration,
    pub(crate) tv: taskvisor::TaskId,
}

/// Result of committing user-owned desired state.
#[derive(Clone, Debug)]
pub(crate) struct DesiredCommit {
    pub(crate) task: Task,
    pub(crate) reconcile: bool,
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
                finished_attempt_by_tv: HashMap::new(),
                next_resource_version: 1,
                terminal_since: HashMap::new(),
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

    fn next_resource_version(inner: &mut TaskStateInner) -> String {
        let version = inner.next_resource_version;
        inner.next_resource_version = inner.next_resource_version.saturating_add(1);
        version.to_string()
    }

    fn index_task(inner: &mut TaskStateInner, task: &Task) {
        let ids = inner.by_slot.entry(task.slot().clone()).or_default();
        if !ids.contains(task.name()) {
            ids.push(task.name().clone());
        }
    }

    fn unindex_task(inner: &mut TaskStateInner, task: &Task) {
        if let Some(ids) = inner.by_slot.get_mut(task.slot()) {
            ids.retain(|name| name != task.name());
            if ids.is_empty() {
                inner.by_slot.remove(task.slot());
            }
        }
    }

    /// Register a manifest unconditionally for test fixtures.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn add_task(&self, manifest: TaskManifest) {
        let mut inner = self.inner.write();
        let name = manifest.name().clone();
        if let Some(previous) = inner.tasks.remove(&name) {
            Self::unindex_task(&mut inner, &previous);
        }
        let mut task = Task::from_manifest(manifest).expect("test manifest must be valid");
        let resource_version = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)
            .expect("store resource version must be valid");
        Self::index_task(&mut inner, &task);
        inner.tasks.insert(name, task);
    }

    /// Create one desired resource. Every retained name conflicts, regardless of phase.
    pub(crate) fn create_desired(
        &self,
        manifest: &TaskManifest,
    ) -> Result<DesiredCommit, CoreError> {
        let mut inner = self.inner.write();
        let name = manifest.name().clone();
        if inner.tasks.contains_key(&name) {
            return Err(CoreError::AlreadyExists(format!(
                "Task resource '{name}' already exists"
            )));
        }

        let mut task = Task::from_manifest(manifest.clone())?;
        let resource_version = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)?;
        Self::index_task(&mut inner, &task);
        inner.terminal_since.remove(&name);
        inner.tasks.insert(name, task.clone());
        Ok(DesiredCommit {
            task,
            reconcile: true,
        })
    }

    /// Apply a manifest by stable name, creating it when absent.
    pub(crate) fn apply_desired(
        &self,
        manifest: &TaskManifest,
    ) -> Result<DesiredCommit, CoreError> {
        manifest.name().validate_format()?;
        manifest.spec().validate()?;
        let mut inner = self.inner.write();
        let name = manifest.name().clone();
        let Some(current) = inner.tasks.get(&name) else {
            drop(inner);
            return self.create_desired(manifest);
        };

        let metadata_changed = current.metadata().labels() != manifest.metadata().labels()
            || current.metadata().annotations() != manifest.metadata().annotations();
        let spec_changed = current.spec() != manifest.spec();
        let retry = !metadata_changed && !spec_changed && current.status().reconciliation_failed();
        if !metadata_changed && !spec_changed && !retry {
            return Ok(DesiredCommit {
                task: current.clone(),
                reconcile: false,
            });
        }

        let previous_slot = current.slot().clone();
        let resource_version = Self::next_resource_version(&mut inner);
        let task = inner
            .tasks
            .get_mut(&name)
            .expect("resource was checked under the same write lock");
        let change = task.apply_desired(
            manifest.metadata().labels().clone(),
            manifest.metadata().annotations().clone(),
            manifest.spec().clone(),
            resource_version.clone(),
        )?;
        if retry {
            task.mark_reconciliation_pending(resource_version)?;
        }

        let task = inner
            .tasks
            .get(&name)
            .expect("applied resource must remain stored")
            .clone();
        if task.slot() != &previous_slot {
            if let Some(ids) = inner.by_slot.get_mut(&previous_slot) {
                ids.retain(|task_name| task_name != &name);
                if ids.is_empty() {
                    inner.by_slot.remove(&previous_slot);
                }
            }
            Self::index_task(&mut inner, &task);
        }
        if change == DesiredChange::Spec || retry {
            inner.terminal_since.remove(&name);
        }
        Ok(DesiredCommit {
            task,
            reconcile: change == DesiredChange::Spec || retry,
        })
    }

    /// Bind a prepared Taskvisor submission to an exact resource generation.
    pub(crate) fn bind_tv(&self, resource: ResourceGeneration, tv: taskvisor::TaskId) -> bool {
        let mut inner = self.inner.write();
        let current = inner.tasks.get(&resource.name).is_some_and(|task| {
            task.uid() == &resource.uid && task.metadata().generation() == resource.generation
        });
        if !current {
            return false;
        }
        if let Some(old) = inner.tv_of.remove(&resource.name) {
            inner.by_tv.remove(&old.tv.get());
            inner.finished_attempt_by_tv.remove(&old.tv.get());
        }
        let binding = RuntimeBinding { resource, tv };
        inner
            .tv_of
            .insert(binding.resource.name.clone(), binding.clone());
        inner.by_tv.insert(tv.get(), binding);
        true
    }

    /// Return whether this exact resource incarnation and desired generation is current.
    pub(crate) fn is_current(&self, resource: &ResourceGeneration) -> bool {
        self.inner
            .read()
            .tasks
            .get(&resource.name)
            .is_some_and(|task| {
                task.uid() == &resource.uid && task.metadata().generation() == resource.generation
            })
    }

    /// Resolve a Taskvisor identity to its exact resource generation.
    pub(crate) fn resolve_tv(&self, tv: u64) -> Option<RuntimeBinding> {
        self.inner.read().by_tv.get(&tv).cloned()
    }

    /// Current Taskvisor binding for a resource name.
    pub(crate) fn binding_for(&self, name: &TaskId) -> Option<RuntimeBinding> {
        self.inner.read().tv_of.get(name).cloned()
    }

    fn unbind_locked(inner: &mut TaskStateInner, name: &TaskId) {
        if let Some(binding) = inner.tv_of.remove(name) {
            inner.by_tv.remove(&binding.tv.get());
            inner.finished_attempt_by_tv.remove(&binding.tv.get());
        }
    }

    pub(crate) fn unbind_tv(&self, tv_raw: u64) -> Option<RuntimeBinding> {
        let mut inner = self.inner.write();
        let binding = inner.by_tv.get(&tv_raw)?.clone();
        if inner
            .tv_of
            .get(&binding.resource.name)
            .is_some_and(|current| current == &binding)
        {
            inner.tv_of.remove(&binding.resource.name);
        }
        inner.by_tv.remove(&tv_raw);
        inner.finished_attempt_by_tv.remove(&tv_raw);
        Some(binding)
    }

    /// Delete a task **and** its run history. Returns `true` if the task existed.
    ///
    /// This is the API-driven full removal. Reconciliation failures retain the
    /// desired resource and therefore never call this method.
    pub(crate) fn delete_task(&self, id: &TaskId) -> bool {
        let mut inner = self.inner.write();
        inner.runs.remove(id);
        inner.terminal_since.remove(id);

        Self::unbind_locked(&mut inner, id);
        if let Some(task) = inner.tasks.remove(id) {
            Self::unindex_task(&mut inner, &task);
            true
        } else {
            false
        }
    }

    fn resource_matches(task: &Task, resource: &ResourceGeneration) -> bool {
        task.name() == &resource.name && task.uid() == &resource.uid
    }

    fn enforce_run_cap(runs: &mut VecDeque<TaskRun>, max: usize) {
        while runs.len() > max {
            let Some(oldest_finished) = runs.iter().position(|run| !run.is_active()) else {
                break;
            };
            runs.remove(oldest_finished);
        }
    }

    /// Record an authoritative Taskvisor attempt start for one exact generation.
    pub(crate) fn transition_attempt_starting(
        &self,
        binding: &RuntimeBinding,
        attempt: u32,
    ) -> bool {
        if attempt == 0 {
            return false;
        }
        let mut inner = self.inner.write();
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource) {
            return false;
        }
        let tv_raw = binding.tv.get();
        if inner
            .finished_attempt_by_tv
            .get(&tv_raw)
            .is_some_and(|finished| attempt <= *finished)
        {
            return false;
        }
        if inner.runs.get(name).is_some_and(|runs| {
            runs.iter()
                .any(|run| run.generation == binding.resource.generation && run.attempt >= attempt)
        }) {
            // A duplicate start, or a start delivered after a later attempt,
            // must not reopen a terminal run or create stale active history.
            return false;
        }
        let updates_current_status = task.metadata().generation() == binding.resource.generation;
        if updates_current_status && attempt <= task.status().attempt {
            return false;
        }

        if updates_current_status {
            let resource_version = Self::next_resource_version(&mut inner);
            let task = inner
                .tasks
                .get_mut(name)
                .expect("resource was checked under the same write lock");
            if let Err(error) =
                task.transition_starting(binding.resource.generation, attempt, resource_version)
            {
                tracing::warn!(task = %name, %error, "ignoring illegal attempt start");
                return false;
            }
            inner.terminal_since.remove(name);
        };

        let max_runs = inner.max_runs_per_task;
        let runs = inner.runs.entry(name.clone()).or_default();
        for run in runs.iter_mut().filter(|run| {
            run.is_active()
                && run.generation == binding.resource.generation
                && run.attempt < attempt
        }) {
            run.finish(
                TaskPhase::Failed,
                Some("run outcome not observed (a later attempt started first)".to_string()),
                None,
            );
        }
        runs.push_back(TaskRun::starting(
            binding.resource.generation,
            attempt,
            binding.resource.workload.clone(),
        ));

        Self::enforce_run_cap(runs, max_runs);
        true
    }

    /// Close the exact attempt described by a Taskvisor attempt event.
    pub(crate) fn transition_attempt_finished(
        &self,
        binding: &RuntimeBinding,
        attempt: u32,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        if attempt == 0 {
            return false;
        }
        let mut inner = self.inner.write();
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource) {
            return false;
        }
        let tv_raw = binding.tv.get();
        if inner
            .finished_attempt_by_tv
            .get(&tv_raw)
            .is_some_and(|finished| attempt <= *finished)
        {
            return false;
        }
        let updates_current_status = task.metadata().generation() == binding.resource.generation
            && attempt >= task.status().attempt;

        let max_runs = inner.max_runs_per_task;
        let (run_error, run_exit_code, run_changed) = {
            let runs = inner.runs.entry(name.clone()).or_default();
            for previous in runs.iter_mut().filter(|run| {
                run.is_active()
                    && run.generation == binding.resource.generation
                    && run.attempt < attempt
            }) {
                previous.finish(
                    TaskPhase::Failed,
                    Some("run outcome not observed (a later attempt finished first)".to_string()),
                    None,
                );
            }
            let run = if let Some(index) = runs.iter().position(|run| {
                run.generation == binding.resource.generation && run.attempt == attempt
            }) {
                &mut runs[index]
            } else {
                runs.push_back(TaskRun::starting(
                    binding.resource.generation,
                    attempt,
                    binding.resource.workload.clone(),
                ));
                runs.back_mut().expect("the run was just appended")
            };
            let run_changed = run.is_active();
            if run_changed {
                run.finish(phase, error, exit_code);
            }
            let diagnostics = (run.error.clone(), run.exit_code, run_changed);
            Self::enforce_run_cap(runs, max_runs);
            diagnostics
        };

        let mut status_changed = false;
        if updates_current_status {
            let resource_version = Self::next_resource_version(&mut inner);
            let task = inner
                .tasks
                .get_mut(name)
                .expect("resource was checked under the same write lock");
            status_changed = match task.transition_finished(
                binding.resource.generation,
                attempt,
                phase,
                run_error,
                run_exit_code,
                resource_version,
            ) {
                Ok(changed) => changed,
                Err(error) => {
                    tracing::warn!(task = %name, %error, "ignoring illegal attempt finish");
                    return false;
                }
            };
            if status_changed && task.status().phase.is_terminal() {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
            }
        }
        let changed = run_changed || status_changed;
        if changed {
            inner.finished_attempt_by_tv.insert(tv_raw, attempt);
        }
        changed
    }

    /// Project a task-level final event without inventing an attempt number.
    pub(crate) fn transition_task_finished(
        &self,
        binding: &RuntimeBinding,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        let mut inner = self.inner.write();
        Self::transition_task_finished_locked(&mut inner, binding, phase, error, exit_code, false)
    }

    /// Mark a generation as accepted by the controller intake path.
    pub(crate) fn mark_observed(&self, resource: &ResourceGeneration) -> bool {
        let mut inner = self.inner.write();
        let Some(task) = inner.tasks.get(&resource.name) else {
            return false;
        };
        if !Self::resource_matches(task, resource)
            || task.metadata().generation() != resource.generation
        {
            return false;
        }
        let resource_version = Self::next_resource_version(&mut inner);
        inner
            .tasks
            .get_mut(&resource.name)
            .expect("resource was checked under the same write lock")
            .mark_observed(resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(task = %resource.name, %error, "could not mark generation observed");
                false
            })
    }

    /// Retain desired state and record that realization failed.
    pub(crate) fn mark_reconciliation_failed(
        &self,
        resource: &ResourceGeneration,
        reason: &'static str,
        message: String,
    ) -> bool {
        let mut inner = self.inner.write();
        let Some(task) = inner.tasks.get(&resource.name) else {
            return false;
        };
        if !Self::resource_matches(task, resource)
            || task.metadata().generation() != resource.generation
        {
            return false;
        }
        let resource_version = Self::next_resource_version(&mut inner);
        inner
            .tasks
            .get_mut(&resource.name)
            .expect("resource was checked under the same write lock")
            .mark_reconciliation_failed(reason, message, resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(task = %resource.name, %error, "could not record reconciliation failure");
                false
            })
    }

    fn transition_task_finished_locked(
        inner: &mut TaskStateInner,
        binding: &RuntimeBinding,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        force: bool,
    ) -> bool {
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource)
            || task.metadata().generation() != binding.resource.generation
        {
            return false;
        }

        let current_phase = task.status().phase;
        let preserve_timeout =
            force && current_phase == TaskPhase::Timeout && phase == TaskPhase::Exhausted;
        let refines_failed = current_phase == TaskPhase::Failed
            && matches!(phase, TaskPhase::Exhausted | TaskPhase::Timeout);
        if preserve_timeout || (!force && current_phase.is_terminal() && !refines_failed) {
            return true;
        }

        let resource_version = Self::next_resource_version(inner);
        let task = inner
            .tasks
            .get_mut(name)
            .expect("resource was checked under the same write lock");
        let result = if force {
            task.reconcile_finished(
                binding.resource.generation,
                phase,
                error.clone(),
                exit_code,
                resource_version,
            )
        } else {
            // TaskFinished has no attempt. Preserve the authoritative attempt
            // already observed from Attempt* events and apply sticky semantics here.
            task.reconcile_finished(
                binding.resource.generation,
                phase,
                error.clone(),
                exit_code,
                resource_version,
            )
        };
        match result {
            Ok(_) => {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
                true
            }
            Err(error) => {
                tracing::warn!(task = %name, %error, "ignoring illegal task finalization");
                false
            }
        }
    }

    /// Atomically finalize the entry bound to taskvisor identity `tv_raw`.
    ///
    /// All checks and mutations happen under one write lock:
    ///
    /// 1. The binding must still be bidirectionally current: `by_tv[tv_raw]` resolves to an entry whose `tv_of` points back at `tv_raw`.
    ///    A stale completion waiter can therefore never touch a newer UID or generation.
    /// 2. The binding is released unconditionally: the waiter fires only once the managed task has fully terminated.
    /// 3. A terminal event-derived phase normally stays sticky, except for the
    ///    model's explicit `Failed` to `Exhausted`/`Timeout` refinement.
    ///    `force` reconciles the resource with an authoritative task outcome;
    ///    a concrete attempt `Timeout` is still preserved over the task's
    ///    generic `Exhausted` disposition.
    ///
    /// Returns the bound entry's id so the caller can evict per-task resources
    /// before releasing the shared lifecycle gate; `None` for a stale binding.
    pub(crate) fn finalize_if_bound(
        &self,
        tv_raw: u64,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        force: bool,
    ) -> Option<TaskId> {
        let mut inner = self.inner.write();

        let binding = inner.by_tv.get(&tv_raw)?.clone();
        if inner
            .tv_of
            .get(&binding.resource.name)
            .is_none_or(|current| current != &binding)
        {
            return None;
        }
        inner.tv_of.remove(&binding.resource.name);
        inner.by_tv.remove(&tv_raw);
        inner.finished_attempt_by_tv.remove(&tv_raw);

        if let Some(runs) = inner.runs.get_mut(&binding.resource.name)
            && let Some(run) = runs
                .iter_mut()
                .rev()
                .find(|run| run.generation == binding.resource.generation && run.is_active())
        {
            run.finish(phase, error.clone(), exit_code);
        }
        Self::transition_task_finished_locked(&mut inner, &binding, phase, error, exit_code, force);
        Some(binding.resource.name)
    }

    /// List all retained runs for a task, oldest first.
    ///
    /// Returns an empty list when the task is unknown or its run history has
    /// already been swept.
    pub fn list_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        let inner = self.inner.read();
        if inner.tasks.get(id).is_some_and(is_internal_task) {
            return Vec::new();
        }
        let mut runs: Vec<TaskRun> = inner
            .runs
            .get(id)
            .map(|runs| runs.iter().cloned().collect())
            .unwrap_or_default();
        runs.sort_by_key(|run| (run.generation, run.attempt));
        runs
    }

    /// Return one task by id.
    pub fn get(&self, id: &TaskId) -> Option<Task> {
        self.get_retained(id).filter(|task| !is_internal_task(task))
    }

    /// Return one retained task including core-owned resources.
    pub(crate) fn get_retained(&self, id: &TaskId) -> Option<Task> {
        let inner = self.inner.read();
        inner.tasks.get(id).cloned()
    }

    /// Return `true` if a task entry currently exists for `id`.
    ///
    /// This is cheaper than [`get`](Self::get) because it does not clone the
    /// task.
    pub fn contains_task(&self, id: &TaskId) -> bool {
        self.inner
            .read()
            .tasks
            .get(id)
            .is_some_and(|task| !is_internal_task(task))
    }

    /// List tasks in a specific slot.
    pub fn list_by_slot(&self, slot: &str) -> Vec<Task> {
        let inner = self.inner.read();

        inner
            .by_slot
            .get(slot)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.tasks.get(id).cloned())
                    .filter(|task| !is_internal_task(task))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all public tasks.
    ///
    /// Solti internal maintenance tasks are excluded.
    pub fn list_all(&self) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| !is_internal_task(task))
            .cloned()
            .collect()
    }

    /// List public tasks that match one phase.
    pub fn list_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| !is_internal_task(task) && task.status().phase == phase)
            .cloned()
            .collect()
    }

    /// Count public tasks per phase.
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
        for task in inner.tasks.values().filter(|task| !is_internal_task(task)) {
            *counts.entry(task.status().phase).or_insert(0) += 1;
        }
        counts
    }

    /// Run a sweep pass.
    ///
    /// Two passes under a single write lock:
    /// 1. Remove finished runs older than `run_ttl`.
    /// 2. Remove terminal tasks that have no remaining runs and whose internal terminal timestamp is older than `task_ttl`.
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
                    && inner.terminal_since.get(*id).is_some_and(|finished| {
                        now.duration_since(*finished)
                            .map(|age| age >= config.task_ttl)
                            .unwrap_or(false)
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired_tasks {
            Self::unbind_locked(&mut inner, id);
            if let Some(task) = inner.tasks.remove(id) {
                Self::unindex_task(&mut inner, &task);
                inner.terminal_since.remove(id);
                tasks_removed += 1;
            }
        }
        if runs_removed > 0 || tasks_removed > 0 {
            debug!(runs_removed, tasks_removed, "state sweep completed");
        }

        (runs_removed, tasks_removed)
    }

    /// Query public tasks with combined filters and pagination.
    ///
    /// Filters are applied inside a single read lock.
    /// When `slot` is specified, uses the `by_slot` index to narrow the scan.
    /// `total` in the result reflects the count *after* filtering, *before* pagination.
    ///
    /// Embedded tasks are normal SDK resources here. Wire-level filtering belongs
    /// to `solti-api`; only solti-core's own maintenance slot is hidden.
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
        self.query_where(q, |_| true)
    }

    /// Query public tasks with an additional caller-owned visibility predicate.
    ///
    /// The predicate is evaluated before `total`, offset and limit. This lets
    /// transport adapters impose their own workload visibility policy without
    /// corrupting pagination, while the ordinary [`query`](Self::query) remains
    /// complete for every SDK workload except core's internal sweep resource.
    pub fn query_where<F>(&self, q: &TaskQuery, predicate: F) -> TaskPage<Task>
    where
        F: Fn(&Task) -> bool,
    {
        let inner = self.inner.read();

        let iter: Box<dyn Iterator<Item = &Task>> = match q.slot() {
            Some(slot) => {
                let ids = inner.by_slot.get(slot.as_str());
                match ids {
                    Some(ids) => Box::new(
                        ids.iter()
                            .filter_map(|id| inner.tasks.get(id))
                            .filter(|task| is_query_visible(task) && predicate(task)),
                    ),
                    None => {
                        return TaskPage {
                            items: vec![],
                            total: 0,
                        };
                    }
                }
            }
            None => Box::new(
                inner
                    .tasks
                    .values()
                    .filter(|task| is_query_visible(task) && predicate(task)),
            ),
        };

        let iter: Box<dyn Iterator<Item = &Task>> = if q.status_filters().is_empty() {
            iter
        } else {
            Box::new(iter.filter(|task| q.matches_phase(&task.status().phase)))
        };

        let mut filtered: Vec<&Task> = iter.collect();
        filtered.sort_by(|a, b| a.name().cmp(b.name()));
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
    pub fn seed_task(&self, id: TaskId, spec: solti_model::TaskSpec) {
        self.add_task(TaskManifest::new(id, spec).expect("test fixture must be valid"));
    }

    /// Transition a seeded task to `Running` (test fixtures only).
    pub fn seed_starting(&self, id: &TaskId) {
        let task = self.get(id).expect("seeded task must exist");
        let resource = ResourceGeneration::from_task(&task);
        let tv = taskvisor::TaskId::for_tests();
        assert!(self.bind_tv(resource.clone(), tv));
        let binding = RuntimeBinding { resource, tv };
        assert!(self.transition_attempt_starting(&binding, 1));
    }

    /// Transition a seeded task to a terminal phase (test fixtures only).
    pub fn seed_finished(
        &self,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) {
        let binding = self.binding_for(id).unwrap_or_else(|| {
            let task = self.get(id).expect("seeded task must exist");
            let resource = ResourceGeneration::from_task(&task);
            let tv = taskvisor::TaskId::for_tests();
            assert!(self.bind_tv(resource.clone(), tv));
            RuntimeBinding { resource, tv }
        });
        let attempt = self
            .get(id)
            .map(|task| task.status().attempt.max(1))
            .unwrap_or(1);
        assert!(self.transition_attempt_finished(&binding, attempt, phase, error, exit_code));
    }
}

/// `true` only for solti-core's own retained sweep resource.
fn is_internal_task(task: &Task) -> bool {
    task.name().as_str() == SWEEP_NAME
}

/// `true` if a task may surface through the paginated query API.
///
/// Only solti-core's own maintenance resource is hidden from general listings.
fn is_query_visible(task: &Task) -> bool {
    !is_internal_task(task)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use solti_model::{
        Annotations, ConditionStatus, EmbeddedSpec, Flag, Labels, SubprocessMode, SubprocessSpec,
        TaskEnv, TaskManifest, TaskSpec, TaskWorkload,
    };

    use super::*;

    fn spec(slot: &str, timeout_ms: u64) -> TaskSpec {
        TaskSpec::builder(
            slot,
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
            timeout_ms,
        )
        .build()
        .expect("valid test spec")
    }

    fn manifest(name: &str, slot: &str, timeout_ms: u64) -> TaskManifest {
        TaskManifest::new(name, spec(slot, timeout_ms)).expect("valid test Task manifest")
    }

    fn create(state: &TaskState, name: &str) -> Task {
        state
            .create_desired(&manifest(name, "slot", 1_000))
            .expect("create")
            .task
    }

    fn bind(state: &TaskState, name: &TaskId) -> RuntimeBinding {
        let resource =
            ResourceGeneration::from_task(&state.get(name).expect("resource must exist"));
        let tv = taskvisor::TaskId::for_tests();
        assert!(state.bind_tv(resource.clone(), tv));
        RuntimeBinding { resource, tv }
    }

    #[test]
    fn create_materializes_server_owned_fields_and_preserves_user_owned_fields() {
        let state = TaskState::new();
        let mut labels = Labels::new();
        labels.insert("team", "runtime");
        let mut annotations = Annotations::new();
        annotations.insert("example.io/note", "kept");

        let incoming = manifest("server-owned", "slot", 1_000)
            .with_labels(labels.clone())
            .with_annotations(annotations.clone());

        let stored = state.create_desired(&incoming).unwrap().task;
        assert!(!stored.uid().as_str().is_empty());
        assert!(!stored.metadata().resource_version().is_empty());
        assert_eq!(stored.metadata().generation(), 1);
        assert_eq!(stored.status().phase, TaskPhase::Pending);
        assert_eq!(stored.status().attempt, 0);
        assert_eq!(stored.metadata().labels(), &labels);
        assert_eq!(stored.metadata().annotations(), &annotations);
    }

    #[test]
    fn create_conflicts_with_every_retained_name_including_terminal() {
        let state = TaskState::new();
        let task = create(&state, "retained");
        let binding = bind(&state, task.name());
        assert_eq!(
            state.finalize_if_bound(
                binding.tv.get(),
                TaskPhase::Canceled,
                Some("canceled".into()),
                None,
                true,
            ),
            Some(task.name().clone())
        );

        let error = state
            .create_desired(&manifest("retained", "slot", 1_000))
            .unwrap_err();
        assert!(matches!(error, CoreError::AlreadyExists(_)));
    }

    #[test]
    fn delete_then_create_assigns_a_new_uid() {
        let state = TaskState::new();
        let first = create(&state, "recreated");
        assert!(state.delete_task(first.name()));
        let second = create(&state, "recreated");

        assert_ne!(first.uid(), second.uid());
        assert_eq!(first.name(), second.name());
    }

    #[test]
    fn exact_apply_is_a_true_noop() {
        let state = TaskState::new();
        let first = create(&state, "noop");
        let result = state.apply_desired(&TaskManifest::from(&first)).unwrap();

        assert!(!result.reconcile);
        assert_eq!(result.task, first);
    }

    #[test]
    fn metadata_only_apply_changes_only_resource_version_and_metadata() {
        let state = TaskState::new();
        let first = create(&state, "metadata");
        let binding = bind(&state, first.name());
        assert!(state.transition_attempt_starting(&binding, 3));
        let before = state.get(first.name()).unwrap();

        let mut labels = Labels::new();
        labels.insert("team", "platform");
        let desired = manifest("metadata", "slot", 1_000).with_labels(labels.clone());
        let result = state.apply_desired(&desired).unwrap();

        assert!(!result.reconcile);
        assert_eq!(result.task.uid(), before.uid());
        assert_eq!(
            result.task.metadata().generation(),
            before.metadata().generation()
        );
        assert_ne!(
            result.task.metadata().resource_version(),
            before.metadata().resource_version()
        );
        assert_eq!(result.task.status(), before.status());
        assert_eq!(result.task.metadata().labels(), &labels);
        assert_eq!(state.binding_for(first.name()), Some(binding));
    }

    #[test]
    fn spec_apply_commits_a_new_pending_generation_without_rollback() {
        let state = TaskState::new();
        let first = create(&state, "changed");
        let old_binding = bind(&state, first.name());
        assert!(state.mark_observed(&old_binding.resource));
        let observed = state.get(first.name()).unwrap();

        let result = state
            .apply_desired(&manifest("changed", "other-slot", 2_000))
            .unwrap();

        assert!(result.reconcile);
        assert_eq!(result.task.uid(), observed.uid());
        assert_eq!(
            result.task.metadata().generation(),
            observed.metadata().generation() + 1
        );
        assert_eq!(
            result.task.status().observed_generation,
            observed.metadata().generation()
        );
        assert_eq!(result.task.status().phase, TaskPhase::Pending);
        assert_eq!(result.task.status().attempt, 0);
        assert_eq!(state.binding_for(first.name()), Some(old_binding));
        assert!(state.list_by_slot("slot").is_empty());
        assert_eq!(state.list_by_slot("other-slot").len(), 1);
    }

    #[test]
    fn apply_missing_creates_a_resource() {
        let state = TaskState::new();
        let result = state
            .apply_desired(&manifest("missing", "slot", 1_000))
            .unwrap();

        assert!(result.reconcile);
        assert!(state.contains_task(result.task.name()));
    }

    #[test]
    fn reconciliation_failure_retains_desired_generation_in_condition() {
        let state = TaskState::new();
        let first = create(&state, "failure");
        let applied = state
            .apply_desired(&manifest("failure", "slot", 2_000))
            .unwrap()
            .task;
        let target = ResourceGeneration::from_task(&applied);

        assert!(state.mark_reconciliation_failed(
            &target,
            "RunnerBuildFailed",
            "runner unavailable".into(),
        ));
        let stored = state.get(applied.name()).unwrap();
        assert_eq!(stored.spec(), applied.spec());
        assert_eq!(stored.metadata().generation(), target.generation);
        assert_eq!(stored.status().observed_generation, target.generation);
        assert_eq!(stored.status().phase, TaskPhase::Pending);
        assert_eq!(stored.status().attempt, 0);
        assert!(stored.status().error.is_none());
        assert_eq!(stored.status().reconciled().status, ConditionStatus::False);
        assert_eq!(stored.status().reconciled().reason, "RunnerBuildFailed");
        assert_eq!(stored.status().reconciled().message, "runner unavailable");
        assert_eq!(stored.uid(), first.uid());
    }

    #[test]
    fn identical_apply_reschedules_only_a_failed_reconciliation() {
        let state = TaskState::new();
        let task = create(&state, "retry");
        let target = ResourceGeneration::from_task(&task);
        assert!(state.mark_reconciliation_failed(
            &target,
            "RunnerBuildFailed",
            "runner unavailable".into(),
        ));

        let retry = state.apply_desired(&TaskManifest::from(&task)).unwrap();
        assert!(retry.reconcile);
        assert_eq!(retry.task.metadata().generation(), target.generation);
        assert_eq!(
            retry.task.status().reconciled().status,
            ConditionStatus::Unknown
        );

        let duplicate = state.apply_desired(&TaskManifest::from(&task)).unwrap();
        assert!(!duplicate.reconcile);
    }

    #[test]
    fn authoritative_attempt_and_generation_are_recorded_in_status_and_run() {
        let state = TaskState::new();
        let task = create(&state, "attempt");
        let binding = bind(&state, task.name());

        assert!(state.transition_attempt_starting(&binding, 4));
        assert!(state.transition_attempt_finished(
            &binding,
            4,
            TaskPhase::Failed,
            Some("exit".into()),
            Some(17),
        ));

        let stored = state.get(task.name()).unwrap();
        assert_eq!(stored.status().attempt, 4);
        assert_eq!(
            stored.status().observed_generation,
            binding.resource.generation
        );
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].generation, binding.resource.generation);
        assert_eq!(runs[0].attempt, 4);
        assert_eq!(runs[0].exit_code, Some(17));
    }

    #[test]
    fn each_run_snapshots_the_workload_gvk_of_its_generation() {
        let state = TaskState::new();
        let first = create(&state, "workload-history");
        let old_binding = bind(&state, first.name());
        assert!(state.transition_attempt_finished(
            &old_binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));

        let routed_workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: "true".into(),
                args: vec![],
            },
            TaskEnv::default(),
            None,
            Flag::enabled(),
        ));
        let desired = TaskManifest::new(
            first.name().clone(),
            TaskSpec::builder("slot", routed_workload, 1_000_u64)
                .build()
                .unwrap(),
        )
        .unwrap();
        let applied = state.apply_desired(&desired).unwrap().task;
        let new_binding = bind(&state, applied.name());
        assert!(state.transition_attempt_starting(&new_binding, 1));

        let runs = state.list_runs(applied.name());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].generation, 1);
        assert_eq!(runs[0].workload.api_version(), "solti.io/v1");
        assert_eq!(runs[0].workload.kind(), "Embedded");
        assert_eq!(runs[1].generation, 2);
        assert_eq!(runs[1].workload.api_version(), "solti.io/v1");
        assert_eq!(runs[1].workload.kind(), "Subprocess");
    }

    #[test]
    fn duplicate_terminal_attempt_is_an_exact_noop() {
        let state = TaskState::new();
        let task = create(&state, "duplicate-finish");
        let binding = bind(&state, task.name());

        assert!(state.transition_attempt_starting(&binding, 1));
        assert!(state.transition_attempt_finished(
            &binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));

        let before_task = state.get(task.name()).unwrap();
        let before_runs = state.list_runs(task.name());
        let terminal_marker = SystemTime::UNIX_EPOCH + Duration::from_secs(17);
        state
            .inner
            .write()
            .terminal_since
            .insert(task.name().clone(), terminal_marker);

        assert!(!state.transition_attempt_finished(
            &binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));

        assert_eq!(state.get(task.name()).unwrap(), before_task);
        assert_eq!(state.list_runs(task.name()), before_runs);
        assert_eq!(
            state.inner.read().terminal_since.get(task.name()),
            Some(&terminal_marker)
        );
    }

    #[test]
    fn duplicate_terminal_attempt_stays_a_noop_after_run_eviction() {
        let state = TaskState::new();
        state.set_max_runs_per_task(0);
        let task = create(&state, "duplicate-evicted-finish");
        let binding = bind(&state, task.name());

        assert!(state.transition_attempt_starting(&binding, 1));
        assert!(state.transition_attempt_finished(
            &binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));
        assert!(state.list_runs(task.name()).is_empty());

        let before_task = state.get(task.name()).unwrap();
        assert!(!state.transition_attempt_finished(
            &binding,
            1,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));
        assert_eq!(state.get(task.name()).unwrap(), before_task);
        assert!(state.list_runs(task.name()).is_empty());
    }

    #[test]
    fn terminal_event_without_start_creates_the_exact_authoritative_run() {
        let state = TaskState::new();
        let task = create(&state, "lost-start");
        let binding = bind(&state, task.name());

        assert!(state.transition_attempt_finished(
            &binding,
            5,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));

        let stored = state.get(task.name()).unwrap();
        assert_eq!(stored.status().attempt, 5);
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].generation, runs[0].attempt), (1, 5));
    }

    #[test]
    fn later_terminal_attempt_closes_an_unresolved_earlier_run() {
        let state = TaskState::new();
        let task = create(&state, "lost-terminal");
        let binding = bind(&state, task.name());
        assert!(state.transition_attempt_starting(&binding, 1));

        assert!(state.transition_attempt_finished(&binding, 2, TaskPhase::Succeeded, None, None,));

        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].attempt, 1);
        assert_eq!(runs[0].phase, TaskPhase::Failed);
        assert!(
            runs[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("later attempt finished"))
        );
        assert_eq!(runs[1].attempt, 2);
        assert_eq!(runs[1].phase, TaskPhase::Succeeded);
    }

    #[test]
    fn authoritative_terminals_without_start_remain_bounded() {
        let state = TaskState::new();
        state.set_max_runs_per_task(2);
        let task = create(&state, "bounded-terminal");
        let binding = bind(&state, task.name());
        for attempt in 1..=3 {
            assert!(state.transition_attempt_finished(
                &binding,
                attempt,
                TaskPhase::Failed,
                Some(format!("attempt {attempt}")),
                None,
            ));
        }

        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].attempt, 2);
        assert_eq!(runs[1].attempt, 3);
    }

    #[test]
    fn old_generation_event_can_close_its_run_but_cannot_mutate_current_status() {
        let state = TaskState::new();
        let first = create(&state, "generation-fence");
        let old = bind(&state, first.name());
        assert!(state.transition_attempt_starting(&old, 1));

        let current = state
            .apply_desired(&manifest("generation-fence", "slot", 2_000))
            .unwrap()
            .task;
        assert_eq!(current.status().phase, TaskPhase::Pending);

        assert!(state.transition_attempt_finished(
            &old,
            1,
            TaskPhase::Failed,
            Some("late".into()),
            Some(1),
        ));

        let stored = state.get(first.name()).unwrap();
        assert_eq!(stored.metadata().generation(), 2);
        assert_eq!(stored.status().phase, TaskPhase::Pending);
        assert_eq!(stored.status().attempt, 0);
        let old_run = &state.list_runs(first.name())[0];
        assert_eq!(old_run.generation, 1);
        assert_eq!(old_run.phase, TaskPhase::Failed);

        let before_task = stored;
        let before_runs = state.list_runs(first.name());
        assert!(!state.transition_attempt_finished(
            &old,
            1,
            TaskPhase::Failed,
            Some("late".into()),
            Some(1),
        ));
        assert_eq!(state.get(first.name()).unwrap(), before_task);
        assert_eq!(state.list_runs(first.name()), before_runs);
    }

    #[test]
    fn stale_uid_cannot_mutate_a_recreated_resource() {
        let state = TaskState::new();
        let first = create(&state, "uid-fence");
        let stale = bind(&state, first.name());
        assert!(state.delete_task(first.name()));
        let replacement = create(&state, "uid-fence");

        assert!(!state.transition_attempt_finished(
            &stale,
            1,
            TaskPhase::Failed,
            Some("stale".into()),
            None,
        ));
        let stored = state.get(replacement.name()).unwrap();
        assert_eq!(stored.uid(), replacement.uid());
        assert_eq!(stored.status().phase, TaskPhase::Pending);
        assert!(state.list_runs(replacement.name()).is_empty());
    }

    #[test]
    fn lower_attempt_cannot_regress_current_status() {
        let state = TaskState::new();
        let task = create(&state, "attempt-fence");
        let binding = bind(&state, task.name());
        assert!(state.transition_attempt_starting(&binding, 3));

        assert!(state.transition_attempt_finished(
            &binding,
            2,
            TaskPhase::Failed,
            Some("late attempt".into()),
            None,
        ));

        let stored = state.get(task.name()).unwrap();
        assert_eq!(stored.status().phase, TaskPhase::Running);
        assert_eq!(stored.status().attempt, 3);
        assert_eq!(
            state
                .list_runs(task.name())
                .iter()
                .find(|run| run.attempt == 2)
                .unwrap()
                .phase,
            TaskPhase::Failed
        );
    }

    #[test]
    fn late_or_duplicate_start_cannot_reopen_attempt_history() {
        let state = TaskState::new();
        let task = create(&state, "start-fence");
        let binding = bind(&state, task.name());
        assert!(state.transition_attempt_starting(&binding, 1));
        assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));

        assert!(!state.transition_attempt_starting(&binding, 1));
        assert!(state.transition_attempt_starting(&binding, 3));
        assert!(!state.transition_attempt_starting(&binding, 2));

        let stored = state.get(task.name()).unwrap();
        assert_eq!(stored.status().phase, TaskPhase::Running);
        assert_eq!(stored.status().attempt, 3);
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].phase, TaskPhase::Succeeded);
        assert_eq!(runs[1].attempt, 3);
    }

    #[test]
    fn embedded_resources_are_public_but_internal_sweep_is_hidden() {
        let state = TaskState::new();
        create(&state, "embedded-public");
        state
            .create_desired(&manifest(
                super::sweep::SWEEP_SLOT,
                super::sweep::SWEEP_SLOT,
                1_000,
            ))
            .unwrap();
        state
            .create_desired(&manifest(
                "user-in-sweep-slot",
                super::sweep::SWEEP_SLOT,
                1_000,
            ))
            .unwrap();

        assert_eq!(state.list_all().len(), 2);
        assert_eq!(state.query(&TaskQuery::new()).total, 2);
        assert_eq!(state.list_by_slot("slot").len(), 1);
        assert_eq!(
            state.list_by_slot(super::sweep::SWEEP_SLOT).len(),
            1,
            "the core-owned resource stays hidden even in its exact slot"
        );
        assert!(state.get(&TaskId::from(super::sweep::SWEEP_NAME)).is_none());
        assert!(
            state
                .list_runs(&TaskId::from(super::sweep::SWEEP_NAME))
                .is_empty()
        );
    }

    #[test]
    fn adapter_predicate_runs_before_total_offset_and_limit() {
        let state = TaskState::new();
        create(&state, "a-visible");
        create(&state, "b-hidden");
        create(&state, "c-visible");
        let query = TaskQuery::new().with_offset(1).with_limit(1);

        let page = state.query_where(&query, |task| task.name().as_str() != "b-hidden");

        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].name().as_str(), "c-visible");
    }

    #[test]
    fn retention_uses_internal_terminal_timestamp() {
        let state = TaskState::new();
        let terminal = create(&state, "expired");
        let binding = bind(&state, terminal.name());
        assert_eq!(
            state.finalize_if_bound(
                binding.tv.get(),
                TaskPhase::Canceled,
                Some("canceled".into()),
                None,
                true,
            ),
            Some(terminal.name().clone())
        );
        create(&state, "pending");

        let config = StateConfig {
            run_ttl: Duration::ZERO,
            task_ttl: Duration::ZERO,
            ..StateConfig::default()
        };
        assert_eq!(state.sweep(&config), (0, 1));
        assert!(!state.contains_task(&TaskId::from("expired")));
        assert!(state.contains_task(&TaskId::from("pending")));
    }

    #[test]
    fn task_level_completion_preserves_authoritative_attempt() {
        let state = TaskState::new();
        let task = create(&state, "final");
        let binding = bind(&state, task.name());
        assert!(state.transition_attempt_starting(&binding, 8));

        assert!(state.transition_task_finished(
            &binding,
            TaskPhase::Canceled,
            Some("controller stopped".into()),
            None,
        ));

        let stored = state.get(task.name()).unwrap();
        assert_eq!(stored.status().attempt, 8);
        assert_eq!(stored.status().phase, TaskPhase::Canceled);
    }
}
