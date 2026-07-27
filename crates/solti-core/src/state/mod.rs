//! In-memory task state.
//!
//! [`TaskState`] stores tasks and execution runs in `Arc<RwLock<_>>`.
//! It is updated from Taskvisor outcomes and cleaned by the retention worker.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::SystemTime,
};

use parking_lot::RwLock;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::{
    Stream,
    wrappers::{BroadcastStream, errors::BroadcastStreamRecvError},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use solti_model::{
    DesiredChange, Slot, Task, TaskContinuation, TaskFilter, TaskId, TaskManifest, TaskPage,
    TaskPhase, TaskQuery, TaskRun, TaskWatchEvent, Uid, WorkloadTypeMeta, WritePreconditions,
};

use crate::{StateConfig, WriteConflict, WritePreconditionViolation, error::CoreError};

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
/// assert!(state.query(&TaskQuery::new()).unwrap().items.is_empty());
/// ```
#[derive(Clone)]
pub struct TaskState {
    inner: Arc<RwLock<TaskStateInner>>,
    watch_stop: CancellationToken,
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
    /// Store incarnation embedded in every opaque resource version.
    resource_version_epoch: String,
    /// Latest resource-version counter committed by this store incarnation.
    resource_version: u64,
    /// Retained Task changes used to resume watches and reconstruct list snapshots.
    watch_history: VecDeque<Arc<RawTaskChange>>,
    /// Serialized Task payload bytes retained in `watch_history`.
    watch_history_bytes: usize,
    /// Maximum serialized Task payload bytes retained in `watch_history`.
    watch_history_byte_budget: usize,
    /// Highest revision no longer available in `watch_history`.
    compacted_through: u64,
    /// Maximum number of retained Task changes.
    watch_history_capacity: usize,
    /// Non-blocking fan-out for live Task changes.
    watch_tx: broadcast::Sender<Arc<RawTaskChange>>,
    /// Internal retention clock for terminal resources.
    terminal_since: HashMap<TaskId, SystemTime>,
    /// Per-task run-history cap (oldest finished runs evicted past this).
    max_runs_per_task: usize,
}

struct RawTaskChange {
    revision: u64,
    previous: Option<Task>,
    current: Option<Task>,
    serialized_bytes: usize,
}

#[derive(Default)]
struct SerializedSizeCounter(usize);

impl Write for SerializedSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type WatchPredicate = Arc<dyn Fn(&Task) -> bool + Send + Sync>;

const WATCH_POLL_BUDGET: usize = 128;

/// Error returned when a Task collection snapshot cannot be read or resumed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CollectionError {
    /// The supplied resource version is malformed or points beyond this store.
    #[error("invalid resourceVersion `{resource_version}`")]
    InvalidResourceVersion {
        /// Resource version supplied by the caller.
        resource_version: String,
    },
    /// The supplied resource version belongs to another store incarnation or
    /// has fallen out of retained collection history.
    #[error("resourceVersion `{resource_version}` has expired")]
    ResourceVersionExpired {
        /// Resource version that can no longer be resumed.
        resource_version: String,
    },
    /// A continuation was used with filters other than those of its first page.
    #[error("continuation filter does not match the query filter")]
    ContinuationFilterMismatch,
    /// The continuation cursor is not present in its retained filtered snapshot.
    #[error("continuation cursor `{name}` is not part of the retained snapshot")]
    ContinuationCursorNotFound {
        /// Task name carried by the continuation cursor.
        name: TaskId,
    },
}

/// Stream of filtered Task changes.
///
/// The stream ends when its owning supervisor shuts down. An
/// [`CollectionError::ResourceVersionExpired`] item is terminal.
#[must_use = "streams do nothing unless polled"]
pub struct TaskWatchSubscription {
    inner: Arc<RwLock<TaskStateInner>>,
    receiver: BroadcastStream<Arc<RawTaskChange>>,
    initial: VecDeque<TaskWatchEvent>,
    initial_revision: Option<u64>,
    replay: VecDeque<Arc<RawTaskChange>>,
    filter: TaskFilter,
    predicate: WatchPredicate,
    epoch: String,
    last_revision: u64,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
    terminal: bool,
}

impl TaskWatchSubscription {
    fn matches(&self, task: &Task) -> bool {
        self.filter.matches(task) && (self.predicate)(task)
    }

    fn event_for(&self, change: &RawTaskChange) -> Option<TaskWatchEvent> {
        let previous_matches = change
            .previous
            .as_ref()
            .is_some_and(|task| self.matches(task));
        let current_matches = change
            .current
            .as_ref()
            .is_some_and(|task| self.matches(task));

        match (previous_matches, current_matches) {
            (false, true) => change.current.clone().map(TaskWatchEvent::Added),
            (true, true) => change.current.clone().map(TaskWatchEvent::Modified),
            (true, false) => {
                let mut task = change.previous.clone()?;
                task.set_resource_version(TaskState::format_resource_version(
                    &self.epoch,
                    change.revision,
                ))
                .expect("store resource version must be valid");
                Some(TaskWatchEvent::Deleted(task))
            }
            (false, false) => None,
        }
    }

    fn recover_after_lag(&mut self) -> Result<(), CollectionError> {
        let inner = self.inner.read();
        if self.last_revision < inner.compacted_through {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    &self.epoch,
                    self.last_revision,
                ),
            });
        }
        self.replay.extend(
            inner
                .watch_history
                .iter()
                .filter(|change| change.revision > self.last_revision)
                .cloned(),
        );
        Ok(())
    }
}

impl Stream for TaskWatchSubscription {
    type Item = Result<TaskWatchEvent, CollectionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        if self.stop.as_mut().poll(cx).is_ready() {
            self.terminal = true;
            return Poll::Ready(None);
        }

        if let Some(event) = self.initial.pop_front() {
            if self.initial.is_empty()
                && let Some(revision) = self.initial_revision.take()
            {
                self.last_revision = revision;
            }
            return Poll::Ready(Some(Ok(event)));
        }
        if let Some(revision) = self.initial_revision.take() {
            self.last_revision = revision;
        }

        let mut processed = 0;
        loop {
            if processed == WATCH_POLL_BUDGET {
                if self.stop.as_mut().poll(cx).is_ready() {
                    self.terminal = true;
                    return Poll::Ready(None);
                }
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            if let Some(change) = self.replay.pop_front() {
                processed += 1;
                self.last_revision = change.revision;
                if let Some(event) = self.event_for(&change) {
                    return Poll::Ready(Some(Ok(event)));
                }
                continue;
            }

            match Pin::new(&mut self.receiver).poll_next(cx) {
                Poll::Ready(Some(Ok(change))) => {
                    processed += 1;
                    if change.revision <= self.last_revision {
                        continue;
                    }
                    self.last_revision = change.revision;
                    if let Some(event) = self.event_for(&change) {
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                    processed += 1;
                    if let Err(error) = self.recover_after_lag() {
                        self.terminal = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                Poll::Ready(None) => {
                    self.terminal = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
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
    /// Use [`try_new`](Self::try_new) when initialization failure must be handled.
    ///
    /// ## Panics
    ///
    /// Panics when the resource-version epoch cannot be generated from OS entropy.
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
        Self::try_new()
            .expect("OS entropy is required to create a TaskState resource-version epoch")
    }

    /// Try to create empty task state with the default per-task run-history cap.
    ///
    /// ## Errors
    ///
    /// Returns [`CoreError::StateInitialization`] when the resource-version epoch
    /// cannot be generated from OS entropy.
    pub fn try_new() -> Result<Self, CoreError> {
        Self::try_with_config(StateConfig::new())
    }

    pub(crate) fn try_with_config(config: StateConfig) -> Result<Self, CoreError> {
        let epoch = Uid::generate()
            .map_err(CoreError::StateInitialization)?
            .to_string();
        Ok(Self::with_epoch(config, epoch))
    }

    fn with_epoch(config: StateConfig, epoch: String) -> Self {
        let (watch_tx, _) = broadcast::channel(config.watch_history_capacity());
        Self {
            inner: Arc::new(RwLock::new(TaskStateInner {
                by_slot: HashMap::new(),
                tasks: HashMap::new(),
                runs: HashMap::new(),
                by_tv: HashMap::new(),
                tv_of: HashMap::new(),
                finished_attempt_by_tv: HashMap::new(),
                resource_version_epoch: epoch,
                resource_version: 0,
                watch_history: VecDeque::new(),
                watch_history_bytes: 0,
                watch_history_byte_budget: config.watch_history_byte_budget(),
                compacted_through: 0,
                watch_history_capacity: config.watch_history_capacity(),
                watch_tx,
                terminal_since: HashMap::new(),
                max_runs_per_task: config.max_runs_per_task(),
            })),
            watch_stop: CancellationToken::new(),
        }
    }

    #[cfg(test)]
    fn set_max_runs_per_task(&self, max: usize) {
        self.inner.write().max_runs_per_task = max;
    }

    fn format_resource_version(epoch: &str, revision: u64) -> String {
        format!("{epoch}:{revision}")
    }

    fn current_resource_version(inner: &TaskStateInner) -> String {
        Self::format_resource_version(&inner.resource_version_epoch, inner.resource_version)
    }

    fn next_resource_version(inner: &mut TaskStateInner) -> (u64, String) {
        inner.resource_version = inner.resource_version.saturating_add(1);
        (
            inner.resource_version,
            Self::current_resource_version(inner),
        )
    }

    fn serialized_task_payload_bytes(previous: Option<&Task>, current: Option<&Task>) -> usize {
        let mut counter = SerializedSizeCounter::default();
        for task in [previous, current].into_iter().flatten() {
            serde_json::to_writer(&mut counter, task)
                .expect("TaskState resources must serialize as JSON");
        }
        counter.0
    }

    fn record_change(
        inner: &mut TaskStateInner,
        revision: u64,
        previous: Option<Task>,
        current: Option<Task>,
    ) {
        if previous == current {
            return;
        }
        let serialized_bytes =
            Self::serialized_task_payload_bytes(previous.as_ref(), current.as_ref());
        let change = Arc::new(RawTaskChange {
            revision,
            previous,
            current,
            serialized_bytes,
        });

        if serialized_bytes > inner.watch_history_byte_budget {
            inner.watch_history.clear();
            inner.watch_history_bytes = 0;
            inner.compacted_through = revision;
        } else {
            while inner.watch_history.len() >= inner.watch_history_capacity
                || inner.watch_history_bytes > inner.watch_history_byte_budget - serialized_bytes
            {
                let compacted = inner
                    .watch_history
                    .pop_front()
                    .expect("a non-empty watch history must satisfy its configured limits");
                inner.watch_history_bytes = inner
                    .watch_history_bytes
                    .checked_sub(compacted.serialized_bytes)
                    .expect("watch history byte accounting must not underflow");
                inner.compacted_through = compacted.revision;
            }
            inner.watch_history_bytes = inner
                .watch_history_bytes
                .checked_add(serialized_bytes)
                .expect("watch history byte accounting must not overflow");
            inner.watch_history.push_back(Arc::clone(&change));
        }

        let _ = inner.watch_tx.send(change);
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
        let previous = inner.tasks.remove(&name);
        if let Some(previous) = previous.as_ref() {
            Self::unindex_task(&mut inner, previous);
        }
        let mut task = Task::from_manifest(manifest).expect("test manifest must be valid");
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)
            .expect("store resource version must be valid");
        Self::index_task(&mut inner, &task);
        inner.tasks.insert(name, task.clone());
        Self::record_change(&mut inner, revision, previous, Some(task));
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
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)?;
        Self::index_task(&mut inner, &task);
        inner.terminal_since.remove(&name);
        inner.tasks.insert(name, task.clone());
        Self::record_change(&mut inner, revision, None, Some(task.clone()));
        Ok(DesiredCommit {
            task,
            reconcile: true,
        })
    }

    /// Apply a manifest by stable name, creating it when absent.
    #[cfg(test)]
    pub(crate) fn apply_desired(
        &self,
        manifest: &TaskManifest,
    ) -> Result<DesiredCommit, CoreError> {
        self.apply_desired_with_preconditions(manifest, &WritePreconditions::new())
    }

    /// Apply a manifest after checking the current resource identity and version.
    pub(crate) fn apply_desired_with_preconditions(
        &self,
        manifest: &TaskManifest,
        preconditions: &WritePreconditions,
    ) -> Result<DesiredCommit, CoreError> {
        manifest.name().validate_format()?;
        manifest.spec().validate()?;
        let mut inner = self.inner.write();
        let name = manifest.name().clone();
        let Some(current) = inner.tasks.get(&name) else {
            if !preconditions.is_empty() {
                return Err(CoreError::NotFound(name.to_string()));
            }
            drop(inner);
            return self.create_desired(manifest);
        };
        Self::check_write_preconditions(current, preconditions)?;

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

        let previous = current.clone();
        let previous_slot = previous.slot().clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
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
        Self::record_change(&mut inner, revision, Some(previous), Some(task.clone()));
        Ok(DesiredCommit {
            task,
            reconcile: change == DesiredChange::Spec || retry,
        })
    }

    pub(crate) fn check_write_preconditions(
        task: &Task,
        preconditions: &WritePreconditions,
    ) -> Result<(), CoreError> {
        let mut violations = Vec::with_capacity(2);
        if let Some(expected) = preconditions.uid()
            && expected != task.uid()
        {
            violations.push(WritePreconditionViolation::Uid {
                expected: expected.clone(),
                actual: task.uid().clone(),
            });
        }
        if let Some(expected) = preconditions.resource_version()
            && expected != task.metadata().resource_version()
        {
            violations.push(WritePreconditionViolation::ResourceVersion {
                expected: expected.to_owned(),
                actual: task.metadata().resource_version().to_owned(),
            });
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Conflict(WriteConflict::new(
                task.name().clone(),
                violations,
            )))
        }
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
            let (revision, _) = Self::next_resource_version(&mut inner);
            Self::record_change(&mut inner, revision, Some(task), None);
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
            runs.iter().any(|run| {
                run.generation() == binding.resource.generation && run.attempt() >= attempt
            })
        }) {
            // A duplicate start, or a start delivered after a later attempt,
            // must not reopen a terminal run or create stale active history.
            return false;
        }
        let updates_current_status = task.metadata().generation() == binding.resource.generation;
        if updates_current_status && attempt <= task.status().attempt() {
            return false;
        }

        let mut task_change = None;
        if updates_current_status {
            let previous = inner
                .tasks
                .get(name)
                .expect("resource was checked under the same write lock")
                .clone();
            let (revision, resource_version) = Self::next_resource_version(&mut inner);
            let task = inner
                .tasks
                .get_mut(name)
                .expect("resource was checked under the same write lock");
            match task.transition_starting(binding.resource.generation, attempt, resource_version) {
                Ok(true) => task_change = Some((revision, previous, task.clone())),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(task = %name, %error, "ignoring illegal attempt start");
                    return false;
                }
            }
            inner.terminal_since.remove(name);
        };

        let max_runs = inner.max_runs_per_task;
        let runs = inner.runs.entry(name.clone()).or_default();
        for run in runs.iter_mut().filter(|run| {
            run.is_active()
                && run.generation() == binding.resource.generation
                && run.attempt() < attempt
        }) {
            run.finish(
                TaskPhase::Failed,
                Some("run outcome not observed (a later attempt started first)".to_string()),
                None,
            )
            .expect("an active run accepts a terminal phase");
        }
        runs.push_back(
            TaskRun::starting(
                binding.resource.generation,
                attempt,
                binding.resource.workload.clone(),
            )
            .expect("validated resource generation and attempt create a run"),
        );

        Self::enforce_run_cap(runs, max_runs);
        if let Some((revision, previous, current)) = task_change {
            Self::record_change(&mut inner, revision, Some(previous), Some(current));
        }
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
        if attempt == 0 || !phase.is_terminal() {
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
            && attempt >= task.status().attempt();

        let max_runs = inner.max_runs_per_task;
        let (run_error, run_exit_code, run_changed) = {
            let runs = inner.runs.entry(name.clone()).or_default();
            for previous in runs.iter_mut().filter(|run| {
                run.is_active()
                    && run.generation() == binding.resource.generation
                    && run.attempt() < attempt
            }) {
                previous
                    .finish(
                        TaskPhase::Failed,
                        Some(
                            "run outcome not observed (a later attempt finished first)".to_string(),
                        ),
                        None,
                    )
                    .expect("an active run accepts a terminal phase");
            }
            let run = if let Some(index) = runs.iter().position(|run| {
                run.generation() == binding.resource.generation && run.attempt() == attempt
            }) {
                &mut runs[index]
            } else {
                runs.push_back(
                    TaskRun::starting(
                        binding.resource.generation,
                        attempt,
                        binding.resource.workload.clone(),
                    )
                    .expect("validated resource generation and attempt create a run"),
                );
                runs.back_mut().expect("the run was just appended")
            };
            let run_changed = run.is_active();
            if run_changed {
                run.finish(phase, error, exit_code)
                    .expect("terminal phase closes an active run");
            }
            let diagnostics = (run.error().map(str::to_owned), run.exit_code(), run_changed);
            Self::enforce_run_cap(runs, max_runs);
            diagnostics
        };

        let mut status_changed = false;
        let mut task_change = None;
        if updates_current_status {
            let previous = inner
                .tasks
                .get(name)
                .expect("resource was checked under the same write lock")
                .clone();
            let (revision, resource_version) = Self::next_resource_version(&mut inner);
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
            if status_changed {
                task_change = Some((revision, previous, task.clone()));
            }
            if status_changed
                && inner
                    .tasks
                    .get(name)
                    .is_some_and(|task| task.status().phase().is_terminal())
            {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
            }
        }
        let changed = run_changed || status_changed;
        if changed {
            inner.finished_attempt_by_tv.insert(tv_raw, attempt);
        }
        if let Some((revision, previous, current)) = task_change {
            Self::record_change(&mut inner, revision, Some(previous), Some(current));
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
        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        let changed = inner
            .tasks
            .get_mut(&resource.name)
            .expect("resource was checked under the same write lock")
            .mark_observed(resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(task = %resource.name, %error, "could not mark generation observed");
                false
            });
        if changed {
            let current = inner
                .tasks
                .get(&resource.name)
                .expect("resource was checked under the same write lock")
                .clone();
            Self::record_change(&mut inner, revision, Some(previous), Some(current));
        }
        changed
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
        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        let changed = inner
            .tasks
            .get_mut(&resource.name)
            .expect("resource was checked under the same write lock")
            .mark_reconciliation_failed(reason, message, resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(task = %resource.name, %error, "could not record reconciliation failure");
                false
            });
        if changed {
            let current = inner
                .tasks
                .get(&resource.name)
                .expect("resource was checked under the same write lock")
                .clone();
            Self::record_change(&mut inner, revision, Some(previous), Some(current));
        }
        changed
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

        let current_phase = task.status().phase();
        let preserve_timeout =
            force && current_phase == TaskPhase::Timeout && phase == TaskPhase::Exhausted;
        let refines_failed = current_phase == TaskPhase::Failed
            && matches!(phase, TaskPhase::Exhausted | TaskPhase::Timeout);
        if preserve_timeout || (!force && current_phase.is_terminal() && !refines_failed) {
            return true;
        }

        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(inner);
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
            Ok(changed) => {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
                if changed {
                    let current = inner
                        .tasks
                        .get(name)
                        .expect("resource was checked under the same write lock")
                        .clone();
                    Self::record_change(inner, revision, Some(previous), Some(current));
                }
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
        if !phase.is_terminal() {
            return None;
        }
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
                .find(|run| run.generation() == binding.resource.generation && run.is_active())
        {
            run.finish(phase, error.clone(), exit_code)
                .expect("terminal phase closes an active run");
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
        let mut runs: Vec<TaskRun> = inner
            .runs
            .get(id)
            .map(|runs| runs.iter().cloned().collect())
            .unwrap_or_default();
        runs.sort_by_key(|run| (run.generation(), run.attempt()));
        runs
    }

    /// Return one task by id.
    pub fn get(&self, id: &TaskId) -> Option<Task> {
        self.get_retained(id)
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
        self.inner.read().tasks.contains_key(id)
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
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all tasks.
    pub fn list_all(&self) -> Vec<Task> {
        let inner = self.inner.read();
        inner.tasks.values().cloned().collect()
    }

    /// List tasks that match one phase.
    pub fn list_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| task.status().phase() == phase)
            .cloned()
            .collect()
    }

    /// Count tasks per phase.
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
        for task in inner.tasks.values() {
            *counts.entry(task.status().phase()).or_insert(0) += 1;
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
            runs.retain(|run| match run.finished_at() {
                Some(finished) => now
                    .duration_since(finished)
                    .map(|age| age < config.run_ttl())
                    .unwrap_or(true),
                None => {
                    task_bound
                        || now
                            .duration_since(run.started_at())
                            .map(|age| age < config.run_ttl())
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
                    && task.status().phase().is_terminal()
                    && inner.runs.get(*id).is_none_or(|runs| runs.is_empty())
                    && inner.terminal_since.get(*id).is_some_and(|finished| {
                        now.duration_since(*finished)
                            .map(|age| age >= config.task_ttl())
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
                let (revision, _) = Self::next_resource_version(&mut inner);
                Self::record_change(&mut inner, revision, Some(task), None);
                tasks_removed += 1;
            }
        }
        if runs_removed > 0 || tasks_removed > 0 {
            debug!(runs_removed, tasks_removed, "state sweep completed");
        }

        (runs_removed, tasks_removed)
    }

    /// Query tasks with combined filters and snapshot-consistent pagination.
    ///
    /// The first page captures the current collection version. A continuation
    /// reads the same retained version even when the live collection changes.
    ///
    /// Embedded tasks are normal SDK resources here. Wire-level filtering belongs
    /// to `solti-api`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::{TaskPhase, TaskQuery};
    ///
    /// let state = TaskState::new();
    /// let query = TaskQuery::new().with_phase(TaskPhase::Running).with_limit(10);
    /// let page = state.query(&query).unwrap();
    ///
    /// assert!(page.items.is_empty());
    /// assert_eq!(page.remaining_item_count, 0);
    /// ```
    pub fn query(&self, q: &TaskQuery) -> Result<TaskPage<Task>, CollectionError> {
        self.query_where(q, |_| true)
    }

    /// Query tasks with an additional caller-owned visibility predicate.
    ///
    /// The predicate is evaluated before the page is cut. It must remain stable
    /// for every continuation in the same snapshot.
    pub fn query_where<F>(
        &self,
        q: &TaskQuery,
        predicate: F,
    ) -> Result<TaskPage<Task>, CollectionError>
    where
        F: Fn(&Task) -> bool,
    {
        let inner = self.inner.read();
        let continuation = q.continuation();
        if continuation.is_some_and(|cursor| cursor.filter() != q.filter()) {
            return Err(CollectionError::ContinuationFilterMismatch);
        }

        let (resource_version, candidates) = match continuation {
            Some(cursor) => (
                cursor.resource_version().to_owned(),
                Self::snapshot_at_resource_version(&inner, cursor.resource_version())?
                    .into_values()
                    .filter(|task| q.matches(task))
                    .collect::<Vec<_>>(),
            ),
            None => (
                Self::current_resource_version(&inner),
                match q.slot() {
                    Some(slot) => inner
                        .by_slot
                        .get(slot.as_str())
                        .into_iter()
                        .flatten()
                        .filter_map(|name| inner.tasks.get(name))
                        .filter(|task| q.matches(task))
                        .cloned()
                        .collect(),
                    None => inner
                        .tasks
                        .values()
                        .filter(|task| q.matches(task))
                        .cloned()
                        .collect(),
                },
            ),
        };
        drop(inner);

        let mut filtered: Vec<Task> = candidates.into_iter().filter(predicate).collect();
        filtered.sort_by(|left, right| left.name().cmp(right.name()));
        let start = match continuation {
            Some(cursor) => filtered
                .binary_search_by(|task| task.name().cmp(cursor.after()))
                .map(|index| index + 1)
                .map_err(|_| CollectionError::ContinuationCursorNotFound {
                    name: cursor.after().clone(),
                })?,
            None => 0,
        };
        let end = start.saturating_add(q.limit()).min(filtered.len());
        let items = filtered[start..end].to_vec();
        let remaining_item_count = filtered.len().saturating_sub(end);
        let continuation = if remaining_item_count > 0 {
            let after = items
                .last()
                .expect("positive page limit with remaining items returns an item")
                .name()
                .clone();
            Some(
                TaskContinuation::new(resource_version.clone(), q.filter().clone(), after)
                    .expect("a state-generated resource version is not empty"),
            )
        } else {
            None
        };

        Ok(TaskPage {
            items,
            resource_version,
            continuation,
            remaining_item_count,
        })
    }

    fn snapshot_at_resource_version(
        inner: &TaskStateInner,
        resource_version: &str,
    ) -> Result<HashMap<TaskId, Task>, CollectionError> {
        let (requested_epoch, requested_revision) = Self::parse_resource_version(resource_version)?;
        if requested_epoch != inner.resource_version_epoch {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision > inner.resource_version {
            return Err(CollectionError::InvalidResourceVersion {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision < inner.compacted_through {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }

        let mut snapshot = inner.tasks.clone();
        for change in inner
            .watch_history
            .iter()
            .rev()
            .take_while(|change| change.revision > requested_revision)
        {
            match (&change.previous, &change.current) {
                (Some(previous), _) => {
                    snapshot.insert(previous.name().clone(), previous.clone());
                }
                (None, Some(current)) => {
                    snapshot.remove(current.name());
                }
                (None, None) => {}
            }
        }
        Ok(snapshot)
    }

    /// Watch Task resources selected by `filter`.
    ///
    /// An absent resource version or `"0"` emits the current matching snapshot
    /// as [`TaskWatchEvent::Added`] events before live changes. An exact opaque
    /// resource version replays later retained changes before live changes.
    pub fn watch(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
    ) -> Result<TaskWatchSubscription, CollectionError> {
        self.watch_where(filter, resource_version, |_| true)
    }

    /// Watch Tasks with an additional caller-owned visibility predicate.
    ///
    /// The predicate participates in transition classification. A Task that
    /// enters visibility is `Added`; one that leaves visibility is `Deleted`.
    pub fn watch_where<F>(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
        predicate: F,
    ) -> Result<TaskWatchSubscription, CollectionError>
    where
        F: Fn(&Task) -> bool + Send + Sync + 'static,
    {
        let predicate: WatchPredicate = Arc::new(predicate);
        let inner = self.inner.read();
        let receiver = BroadcastStream::new(inner.watch_tx.subscribe());
        let epoch = inner.resource_version_epoch.clone();
        let mut initial = VecDeque::new();
        let mut initial_revision = None;
        let mut replay = VecDeque::new();
        let last_revision;

        match resource_version {
            None | Some("0") => {
                let mut tasks: Vec<Task> = inner
                    .tasks
                    .values()
                    .filter(|task| filter.matches(task) && predicate(task))
                    .cloned()
                    .collect();
                tasks.sort_by(|left, right| left.name().cmp(right.name()));
                initial.extend(tasks.into_iter().map(TaskWatchEvent::Added));
                initial_revision = Some(inner.resource_version);
                last_revision = 0;
            }
            Some(value) => {
                let (requested_epoch, requested_revision) = Self::parse_resource_version(value)?;
                if requested_epoch != epoch {
                    return Err(CollectionError::ResourceVersionExpired {
                        resource_version: value.to_string(),
                    });
                }
                if requested_revision > inner.resource_version {
                    return Err(CollectionError::InvalidResourceVersion {
                        resource_version: value.to_string(),
                    });
                }
                if requested_revision < inner.compacted_through {
                    return Err(CollectionError::ResourceVersionExpired {
                        resource_version: value.to_string(),
                    });
                }
                replay.extend(
                    inner
                        .watch_history
                        .iter()
                        .filter(|change| change.revision > requested_revision)
                        .cloned(),
                );
                last_revision = requested_revision;
            }
        }
        drop(inner);

        Ok(TaskWatchSubscription {
            inner: Arc::clone(&self.inner),
            receiver,
            initial,
            initial_revision,
            replay,
            filter: filter.clone(),
            predicate,
            epoch,
            last_revision,
            stop: Box::pin(self.watch_stop.clone().cancelled_owned()),
            terminal: false,
        })
    }

    fn parse_resource_version(value: &str) -> Result<(&str, u64), CollectionError> {
        let invalid = || CollectionError::InvalidResourceVersion {
            resource_version: value.to_string(),
        };
        if value.is_empty() {
            return Err(invalid());
        }
        let (epoch, revision) = value.split_once(':').ok_or_else(&invalid)?;
        if epoch.is_empty() || revision.is_empty() || revision.contains(':') {
            return Err(invalid());
        }
        let revision = revision.parse::<u64>().map_err(|_| invalid())?;
        Ok((epoch, revision))
    }

    pub(crate) fn close_watches(&self) {
        self.watch_stop.cancel();
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
            .map(|task| task.status().attempt().max(1))
            .unwrap_or(1);
        assert!(self.transition_attempt_finished(&binding, attempt, phase, error, exit_code));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };

    use solti_model::{
        Annotations, ConditionStatus, EmbeddedSpec, Flag, LabelSelector, Labels, SubprocessMode,
        SubprocessSpec, TaskEnv, TaskManifest, TaskSpec, TaskWorkload,
    };
    use tokio_stream::StreamExt;

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

    fn journal_task(name: &str, revision: u64, annotation_bytes: usize) -> Task {
        let mut task_manifest = manifest(name, "slot", 1_000);
        if annotation_bytes > 0 {
            let mut annotations = Annotations::new();
            annotations.insert("example.io/payload", "x".repeat(annotation_bytes));
            task_manifest = task_manifest.with_annotations(annotations).unwrap();
        }
        let mut task = Task::from_manifest(task_manifest).unwrap();
        task.set_resource_version(format!("epoch:{revision}"))
            .unwrap();
        task
    }

    fn record_current_change(state: &TaskState, task: Task) {
        let mut inner = state.inner.write();
        let (revision, resource_version) = TaskState::next_resource_version(&mut inner);
        assert_eq!(task.metadata().resource_version(), resource_version);
        TaskState::record_change(&mut inner, revision, None, Some(task));
    }

    fn bind(state: &TaskState, name: &TaskId) -> RuntimeBinding {
        let resource =
            ResourceGeneration::from_task(&state.get(name).expect("resource must exist"));
        let tv = taskvisor::TaskId::for_tests();
        assert!(state.bind_tv(resource.clone(), tv));
        RuntimeBinding { resource, tv }
    }

    #[test]
    fn try_new_creates_empty_state() {
        let state = TaskState::try_new().expect("OS entropy is available");

        assert!(state.list_all().is_empty());
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
            .unwrap()
            .with_annotations(annotations.clone())
            .unwrap();

        let stored = state.create_desired(&incoming).unwrap().task;
        assert!(!stored.uid().as_str().is_empty());
        assert!(!stored.metadata().resource_version().is_empty());
        assert_eq!(stored.metadata().generation(), 1);
        assert_eq!(stored.status().phase(), TaskPhase::Pending);
        assert_eq!(stored.status().attempt(), 0);
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
    fn checked_apply_accepts_matching_uid_and_resource_version() {
        let state = TaskState::new();
        let first = create(&state, "checked");
        let preconditions = WritePreconditions::from_task(&first).unwrap();

        let result = state
            .apply_desired_with_preconditions(&TaskManifest::from(&first), &preconditions)
            .unwrap();

        assert!(!result.reconcile);
        assert_eq!(result.task, first);
    }

    #[test]
    fn checked_apply_rejects_every_mismatch_without_consuming_a_version() {
        let state = TaskState::new();
        let first = create(&state, "stale");
        let preconditions = WritePreconditions::new()
            .with_uid(Uid::new("stale-uid").unwrap())
            .with_resource_version("stale-version")
            .unwrap();

        let error = state
            .apply_desired_with_preconditions(&TaskManifest::from(&first), &preconditions)
            .unwrap_err();
        let CoreError::Conflict(conflict) = error else {
            panic!("expected conflict");
        };
        assert_eq!(conflict.name(), first.name());
        assert_eq!(conflict.violations().len(), 2);
        assert_eq!(state.get(first.name()), Some(first.clone()));

        let mut labels = Labels::new();
        labels.insert("changed", "true");
        let changed = TaskManifest::from(&first).with_labels(labels).unwrap();
        let applied = state.apply_desired(&changed).unwrap().task;
        assert_eq!(
            TaskState::parse_resource_version(applied.metadata().resource_version())
                .unwrap()
                .1,
            2
        );
    }

    #[test]
    fn checked_apply_does_not_create_a_missing_resource() {
        let state = TaskState::new();
        let desired = manifest("missing-checked", "slot", 1_000);
        let preconditions = WritePreconditions::new()
            .with_resource_version("1")
            .unwrap();

        let error = state
            .apply_desired_with_preconditions(&desired, &preconditions)
            .unwrap_err();

        assert!(matches!(error, CoreError::NotFound(_)));
        assert!(state.get(desired.name()).is_none());
    }

    #[test]
    fn stale_uid_cannot_update_a_recreated_resource() {
        let state = TaskState::new();
        let first = create(&state, "recreated-checked");
        let stale = WritePreconditions::from_task(&first).unwrap();
        assert!(state.delete_task(first.name()));
        let replacement = create(&state, "recreated-checked");

        let error = state
            .apply_desired_with_preconditions(&TaskManifest::from(&replacement), &stale)
            .unwrap_err();

        assert!(matches!(error, CoreError::Conflict(_)));
        assert_eq!(state.get(replacement.name()), Some(replacement));
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
        let desired = manifest("metadata", "slot", 1_000)
            .with_labels(labels.clone())
            .unwrap();
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
            result.task.status().observed_generation(),
            observed.metadata().generation()
        );
        assert_eq!(result.task.status().phase(), TaskPhase::Pending);
        assert_eq!(result.task.status().attempt(), 0);
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
        assert_eq!(stored.status().observed_generation(), target.generation);
        assert_eq!(stored.status().phase(), TaskPhase::Pending);
        assert_eq!(stored.status().attempt(), 0);
        assert!(stored.status().error().is_none());
        assert_eq!(
            stored.status().reconciled().status(),
            ConditionStatus::False
        );
        assert_eq!(stored.status().reconciled().reason(), "RunnerBuildFailed");
        assert_eq!(stored.status().reconciled().message(), "runner unavailable");
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
            retry.task.status().reconciled().status(),
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
        assert_eq!(stored.status().attempt(), 4);
        assert_eq!(
            stored.status().observed_generation(),
            binding.resource.generation
        );
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].generation(), binding.resource.generation);
        assert_eq!(runs[0].attempt(), 4);
        assert_eq!(runs[0].exit_code(), Some(17));
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
        assert_eq!(runs[0].generation(), 1);
        assert_eq!(runs[0].workload().api_version(), "solti.io/v1");
        assert_eq!(runs[0].workload().kind(), "Embedded");
        assert_eq!(runs[1].generation(), 2);
        assert_eq!(runs[1].workload().api_version(), "solti.io/v1");
        assert_eq!(runs[1].workload().kind(), "Subprocess");
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
        assert_eq!(stored.status().attempt(), 5);
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 5));
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
        assert_eq!(runs[0].attempt(), 1);
        assert_eq!(runs[0].phase(), TaskPhase::Failed);
        assert!(
            runs[0]
                .error()
                .is_some_and(|error| error.contains("later attempt finished"))
        );
        assert_eq!(runs[1].attempt(), 2);
        assert_eq!(runs[1].phase(), TaskPhase::Succeeded);
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
        assert_eq!(runs[0].attempt(), 2);
        assert_eq!(runs[1].attempt(), 3);
    }

    #[test]
    fn zero_run_cap_keeps_only_active_runs() {
        let state = TaskState::new();
        state.set_max_runs_per_task(0);
        let task = create(&state, "no-history");
        let binding = bind(&state, task.name());

        assert!(state.transition_attempt_starting(&binding, 1));
        assert_eq!(state.list_runs(task.name()).len(), 1);

        assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));
        assert!(state.list_runs(task.name()).is_empty());
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
        assert_eq!(current.status().phase(), TaskPhase::Pending);

        assert!(state.transition_attempt_finished(
            &old,
            1,
            TaskPhase::Failed,
            Some("late".into()),
            Some(1),
        ));

        let stored = state.get(first.name()).unwrap();
        assert_eq!(stored.metadata().generation(), 2);
        assert_eq!(stored.status().phase(), TaskPhase::Pending);
        assert_eq!(stored.status().attempt(), 0);
        let old_run = &state.list_runs(first.name())[0];
        assert_eq!(old_run.generation(), 1);
        assert_eq!(old_run.phase(), TaskPhase::Failed);

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
        assert_eq!(stored.status().phase(), TaskPhase::Pending);
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
        assert_eq!(stored.status().phase(), TaskPhase::Running);
        assert_eq!(stored.status().attempt(), 3);
        assert_eq!(
            state
                .list_runs(task.name())
                .iter()
                .find(|run| run.attempt() == 2)
                .unwrap()
                .phase(),
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
        assert_eq!(stored.status().phase(), TaskPhase::Running);
        assert_eq!(stored.status().attempt(), 3);
        let runs = state.list_runs(task.name());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].phase(), TaskPhase::Succeeded);
        assert_eq!(runs[1].attempt(), 3);
    }

    #[test]
    fn every_embedded_resource_name_and_slot_is_public() {
        let state = TaskState::new();
        create(&state, "embedded-public");
        state
            .create_desired(&manifest("solti-state-sweep", "solti-state-sweep", 1_000))
            .unwrap();
        state
            .create_desired(&manifest("user-in-sweep-slot", "solti-state-sweep", 1_000))
            .unwrap();

        assert_eq!(state.list_all().len(), 3);
        assert_eq!(state.query(&TaskQuery::new()).unwrap().items.len(), 3);
        assert_eq!(state.list_by_slot("slot").len(), 1);
        assert_eq!(state.list_by_slot("solti-state-sweep").len(), 2);
        assert!(
            state
                .get(&TaskId::new("solti-state-sweep").unwrap())
                .is_some()
        );
    }

    #[test]
    fn adapter_predicate_runs_before_pagination() {
        let state = TaskState::new();
        create(&state, "a-visible");
        create(&state, "b-hidden");
        create(&state, "c-visible");
        let query = TaskQuery::new().with_limit(1);

        let first = state
            .query_where(&query, |task| task.name().as_str() != "b-hidden")
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].name().as_str(), "a-visible");
        assert_eq!(first.remaining_item_count, 1);

        let second = state
            .query_where(
                &query.with_continuation(first.continuation.unwrap()),
                |task| task.name().as_str() != "b-hidden",
            )
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].name().as_str(), "c-visible");
        assert_eq!(second.remaining_item_count, 0);
        assert!(second.continuation.is_none());
    }

    #[test]
    fn slot_labels_and_multiple_phases_filter_before_pagination() {
        let state = TaskState::new();
        for (name, environment, tier) in [
            ("a-match", "production", "frontend"),
            ("b-no-match", "development", "frontend"),
            ("c-match", "production", "backend"),
        ] {
            let mut labels = Labels::new();
            labels
                .insert("environment", environment)
                .insert("tier", tier);
            state
                .create_desired(
                    &manifest(name, "primary", 1_000)
                        .with_labels(labels)
                        .unwrap(),
                )
                .unwrap();
        }
        let running = state.get(&TaskId::new("c-match").unwrap()).unwrap();
        let binding = bind(&state, running.name());
        assert!(state.transition_attempt_starting(&binding, 1));

        let selector: LabelSelector = "environment=production,tier in (frontend,backend)"
            .parse()
            .unwrap();
        let query = TaskQuery::new()
            .with_slot(Slot::new("primary").unwrap())
            .with_phases([TaskPhase::Pending, TaskPhase::Running])
            .with_label_selector(selector)
            .unwrap()
            .with_limit(1);

        let first = state.query(&query).unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].name().as_str(), "a-match");
        assert_eq!(first.remaining_item_count, 1);

        let second = state
            .query(&query.with_continuation(first.continuation.unwrap()))
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].name().as_str(), "c-match");
        assert_eq!(second.remaining_item_count, 0);
    }

    #[test]
    fn metadata_apply_changes_label_query_membership_immediately() {
        let state = TaskState::new();
        let first = create(&state, "label-change");
        let query = TaskQuery::new()
            .with_label_selector("environment=production".parse().unwrap())
            .unwrap();
        assert!(state.query(&query).unwrap().items.is_empty());

        let mut labels = Labels::new();
        labels.insert("environment", "production");
        let applied = state
            .apply_desired(&TaskManifest::from(&first).with_labels(labels).unwrap())
            .unwrap()
            .task;

        assert_eq!(
            applied.metadata().generation(),
            first.metadata().generation()
        );
        assert_ne!(
            applied.metadata().resource_version(),
            first.metadata().resource_version()
        );
        assert_eq!(state.query(&query).unwrap().items.len(), 1);
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

        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        assert_eq!(state.sweep(&config), (0, 1));
        assert!(!state.contains_task(&TaskId::new("expired").unwrap()));
        assert!(state.contains_task(&TaskId::new("pending").unwrap()));
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
        assert_eq!(stored.status().attempt(), 8);
        assert_eq!(stored.status().phase(), TaskPhase::Canceled);
    }

    #[test]
    fn watch_history_retains_a_change_at_the_exact_byte_budget() {
        let task = journal_task("exact-budget", 1, 0);
        let serialized_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
        let config = StateConfig::new()
            .try_with_watch_history_byte_budget(serialized_bytes)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());

        record_current_change(&state, task);

        let inner = state.inner.read();
        assert_eq!(inner.watch_history.len(), 1);
        assert_eq!(inner.watch_history_bytes, serialized_bytes);
        assert_eq!(
            inner.watch_history.front().unwrap().serialized_bytes,
            serialized_bytes
        );
        assert_eq!(inner.compacted_through, 0);
    }

    #[test]
    fn watch_history_byte_budget_can_evict_multiple_changes() {
        let first = journal_task("small-first", 1, 0);
        let second = journal_task("small-second", 2, 0);
        let third = journal_task("large-third", 3, 4 * 1024);
        let first_bytes = TaskState::serialized_task_payload_bytes(None, Some(&first));
        let second_bytes = TaskState::serialized_task_payload_bytes(None, Some(&second));
        let third_bytes = TaskState::serialized_task_payload_bytes(None, Some(&third));
        assert!(first_bytes + second_bytes <= third_bytes);
        let config = StateConfig::new()
            .try_with_watch_history_byte_budget(third_bytes)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());

        record_current_change(&state, first);
        record_current_change(&state, second);
        record_current_change(&state, third);

        let inner = state.inner.read();
        assert_eq!(inner.watch_history.len(), 1);
        assert_eq!(inner.watch_history.front().unwrap().revision, 3);
        assert_eq!(inner.watch_history_bytes, third_bytes);
        assert_eq!(inner.compacted_through, 2);
    }

    #[tokio::test]
    async fn oversized_change_is_live_but_not_retained() {
        let first = journal_task("retained-before-oversized", 1, 0);
        let task = journal_task("oversized-live", 2, 4 * 1024);
        let serialized_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
        let config = StateConfig::new()
            .try_with_watch_history_byte_budget(serialized_bytes - 1)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());
        record_current_change(&state, first);
        let mut watch = state.watch(&TaskFilter::new(), Some("epoch:1")).unwrap();

        record_current_change(&state, task.clone());

        {
            let inner = state.inner.read();
            assert!(inner.watch_history.is_empty());
            assert_eq!(inner.watch_history_bytes, 0);
            assert_eq!(inner.compacted_through, 2);
        }
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(task)
        );
        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("epoch:1")),
            Err(CollectionError::ResourceVersionExpired { .. })
        ));
    }

    #[test]
    fn continuation_expires_after_byte_budget_compaction() {
        let config = StateConfig::new()
            .try_with_watch_history_byte_budget(1)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());
        create(&state, "a-first");
        create(&state, "b-second");
        let query = TaskQuery::new().with_limit(1);
        let first_page = state.query(&query).unwrap();
        let resource_version = first_page.resource_version.clone();
        let continuation = first_page.continuation.unwrap();

        create(&state, "c-third");

        assert_eq!(
            state
                .query(&query.with_continuation(continuation))
                .unwrap_err(),
            CollectionError::ResourceVersionExpired { resource_version }
        );
    }

    #[test]
    fn list_snapshot_carries_the_atomic_collection_version() {
        let state = TaskState::new();
        let empty = state.query(&TaskQuery::new()).unwrap();
        let (epoch, revision) = TaskState::parse_resource_version(&empty.resource_version).unwrap();
        assert!(!epoch.is_empty());
        assert_eq!(revision, 0);

        let task = create(&state, "versioned-list");
        let page = state.query(&TaskQuery::new()).unwrap();
        assert_eq!(page.resource_version, task.metadata().resource_version());
        assert_eq!(page.items, vec![task]);
    }

    #[test]
    fn continuation_reads_the_first_page_snapshot_after_live_changes() {
        let state = TaskState::new();
        let first_task = create(&state, "b-first");
        let second_task = create(&state, "c-second");
        let query = TaskQuery::new().with_limit(1);

        let first_page = state.query(&query).unwrap();
        assert_eq!(first_page.items, vec![first_task]);
        assert_eq!(first_page.remaining_item_count, 1);
        let continuation = first_page.continuation.unwrap();

        create(&state, "a-added-later");
        state
            .apply_desired(&manifest("c-second", "changed", 2_000))
            .unwrap();
        assert!(state.delete_task(second_task.name()));

        let second_page = state.query(&query.with_continuation(continuation)).unwrap();
        assert_eq!(second_page.resource_version, first_page.resource_version);
        assert_eq!(second_page.items, vec![second_task]);
        assert_eq!(second_page.remaining_item_count, 0);
        assert!(second_page.continuation.is_none());
    }

    #[test]
    fn continuation_is_bound_to_its_filter_and_last_returned_name() {
        let state = TaskState::new();
        create(&state, "a-first");
        create(&state, "b-second");
        let query = TaskQuery::new().with_limit(1);
        let first_page = state.query(&query).unwrap();
        let continuation = first_page.continuation.unwrap();

        let mismatch = query
            .clone()
            .with_phase(TaskPhase::Running)
            .with_continuation(continuation.clone());
        assert_eq!(
            state.query(&mismatch).unwrap_err(),
            CollectionError::ContinuationFilterMismatch
        );

        let missing = TaskContinuation::new(
            continuation.resource_version(),
            query.filter().clone(),
            TaskId::new("missing-cursor").unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.query(&query.with_continuation(missing)).unwrap_err(),
            CollectionError::ContinuationCursorNotFound {
                name: TaskId::new("missing-cursor").unwrap(),
            }
        );
    }

    #[test]
    fn continuation_reports_invalid_and_expired_snapshots() {
        let config = StateConfig::new()
            .try_with_watch_history_capacity(1)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());
        create(&state, "a-first");
        create(&state, "b-second");
        let query = TaskQuery::new().with_limit(1);
        let first_page = state.query(&query).unwrap();
        let continuation = first_page.continuation.unwrap();

        let invalid = TaskContinuation::new(
            "epoch:99",
            query.filter().clone(),
            continuation.after().clone(),
        )
        .unwrap();
        assert_eq!(
            state
                .query(&query.clone().with_continuation(invalid))
                .unwrap_err(),
            CollectionError::InvalidResourceVersion {
                resource_version: "epoch:99".to_string(),
            }
        );

        create(&state, "c-third");
        create(&state, "d-fourth");
        assert_eq!(
            state
                .query(&query.with_continuation(continuation))
                .unwrap_err(),
            CollectionError::ResourceVersionExpired {
                resource_version: first_page.resource_version,
            }
        );
    }

    #[tokio::test]
    async fn list_then_watch_replays_every_change_after_the_snapshot() {
        let state = TaskState::new();
        let listed = state.query(&TaskQuery::new()).unwrap();
        let created = create(&state, "created-in-gap");

        let mut watch = state
            .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
            .unwrap();
        let event = watch.next().await.unwrap().unwrap();

        assert_eq!(event, TaskWatchEvent::Added(created));
    }

    #[tokio::test]
    async fn watch_without_version_emits_sorted_snapshot_then_live_changes() {
        let state = TaskState::new();
        let second = create(&state, "b-snapshot");
        let first = create(&state, "a-snapshot");
        let mut watch = state.watch(&TaskFilter::new(), None).unwrap();

        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(first)
        );
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(second)
        );

        let live = create(&state, "c-live");
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(live)
        );
    }

    #[test]
    fn watch_rejects_expired_invalid_and_foreign_versions() {
        let config = StateConfig::new()
            .try_with_watch_history_capacity(1)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());
        create(&state, "first");
        create(&state, "second");

        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("epoch:0")),
            Err(CollectionError::ResourceVersionExpired { .. })
        ));
        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("another:2")),
            Err(CollectionError::ResourceVersionExpired { .. })
        ));
        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("")),
            Err(CollectionError::InvalidResourceVersion { .. })
        ));
        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("epoch:not-a-number")),
            Err(CollectionError::InvalidResourceVersion { .. })
        ));
        assert!(matches!(
            state.watch(&TaskFilter::new(), Some("epoch:3")),
            Err(CollectionError::InvalidResourceVersion { .. })
        ));
    }

    #[tokio::test]
    async fn selector_membership_changes_map_to_added_modified_and_deleted() {
        let state = TaskState::new();
        let first = create(&state, "selector-transition");
        let filter = TaskFilter::new()
            .with_label_selector("environment=production".parse().unwrap())
            .unwrap();
        let listed = state.query(&TaskQuery::new()).unwrap();
        let mut watch = state
            .watch(&filter, Some(listed.resource_version.as_str()))
            .unwrap();

        let mut labels = Labels::new();
        labels.insert("environment", "production");
        let added = state
            .apply_desired(&TaskManifest::from(&first).with_labels(labels).unwrap())
            .unwrap()
            .task;
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(added.clone())
        );

        let mut annotations = Annotations::new();
        annotations.insert("example.io/revision", "2");
        let modified = state
            .apply_desired(
                &TaskManifest::from(&added)
                    .with_annotations(annotations)
                    .unwrap(),
            )
            .unwrap()
            .task;
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Modified(modified.clone())
        );

        let current = state
            .apply_desired(
                &TaskManifest::from(&modified)
                    .with_labels(Labels::new())
                    .unwrap(),
            )
            .unwrap()
            .task;
        let deleted = watch.next().await.unwrap().unwrap();
        let TaskWatchEvent::Deleted(deleted) = deleted else {
            panic!("selector exit must be Deleted");
        };
        assert_eq!(deleted.name(), current.name());
        assert_eq!(deleted.metadata().labels(), modified.metadata().labels());
        assert_eq!(
            deleted.metadata().resource_version(),
            current.metadata().resource_version()
        );
    }

    #[tokio::test]
    async fn adapter_predicate_participates_in_watch_transitions() {
        let state = TaskState::new();
        let first = create(&state, "visibility-transition");
        let listed = state.query(&TaskQuery::new()).unwrap();
        let mut watch = state
            .watch_where(
                &TaskFilter::new(),
                Some(listed.resource_version.as_str()),
                |task| task.spec().timeout().as_millis() <= 1_000,
            )
            .unwrap();

        let hidden = state
            .apply_desired(&TaskManifest::new(first.name().as_str(), spec("slot", 2_000)).unwrap())
            .unwrap()
            .task;
        let TaskWatchEvent::Deleted(deleted) = watch.next().await.unwrap().unwrap() else {
            panic!("leaving adapter visibility must be Deleted");
        };
        assert_eq!(deleted.name(), hidden.name());
        assert_eq!(
            deleted.metadata().resource_version(),
            hidden.metadata().resource_version()
        );

        let visible = state
            .apply_desired(&TaskManifest::new(first.name().as_str(), spec("slot", 1_000)).unwrap())
            .unwrap()
            .task;
        assert_eq!(
            watch.next().await.unwrap().unwrap(),
            TaskWatchEvent::Added(visible)
        );
    }

    #[tokio::test]
    async fn delete_and_sweep_each_publish_one_deleted_event() {
        let state = TaskState::new();
        let deleted = create(&state, "api-deleted");
        let listed = state.query(&TaskQuery::new()).unwrap();
        let mut watch = state
            .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
            .unwrap();
        assert!(state.delete_task(deleted.name()));
        let TaskWatchEvent::Deleted(event) = watch.next().await.unwrap().unwrap() else {
            panic!("delete must emit Deleted");
        };
        assert_eq!(event.name(), deleted.name());
        assert_ne!(
            event.metadata().resource_version(),
            deleted.metadata().resource_version()
        );

        let expired = create(&state, "sweep-deleted");
        let binding = bind(&state, expired.name());
        assert_eq!(
            state.finalize_if_bound(
                binding.tv.get(),
                TaskPhase::Canceled,
                Some("canceled".into()),
                None,
                true,
            ),
            Some(expired.name().clone())
        );
        let listed = state.query(&TaskQuery::new()).unwrap();
        let mut watch = state
            .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
            .unwrap();
        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        assert_eq!(state.sweep(&config), (0, 1));
        let TaskWatchEvent::Deleted(event) = watch.next().await.unwrap().unwrap() else {
            panic!("sweep must emit Deleted");
        };
        assert_eq!(event.name(), expired.name());
    }

    #[tokio::test]
    async fn no_op_apply_and_run_only_change_publish_nothing() {
        let state = TaskState::new();
        let first = create(&state, "no-watch-noop");
        let binding = bind(&state, first.name());
        assert!(state.transition_attempt_starting(&binding, 1));

        let changed = TaskManifest::new(first.name().as_str(), spec("slot", 2_000)).unwrap();
        let current = state.apply_desired(&changed).unwrap().task;
        let listed = state.query(&TaskQuery::new()).unwrap();
        let mut watch = state
            .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
            .unwrap();

        let noop = state.apply_desired(&TaskManifest::from(&current)).unwrap();
        assert!(!noop.reconcile);
        assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));
        assert_eq!(
            state.query(&TaskQuery::new()).unwrap().resource_version,
            listed.resource_version
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), watch.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn lag_is_terminal_once_the_resume_point_is_compacted() {
        let config = StateConfig::new()
            .try_with_watch_history_capacity(1)
            .unwrap();
        let state = TaskState::with_epoch(config, "epoch".to_string());
        let mut watch = state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();

        create(&state, "first");
        create(&state, "second");

        assert!(matches!(
            watch.next().await,
            Some(Err(CollectionError::ResourceVersionExpired { .. }))
        ));
        assert!(watch.next().await.is_none());
    }

    #[test]
    fn irrelevant_live_events_yield_after_the_poll_budget() {
        struct CountingWake(AtomicUsize);

        impl Wake for CountingWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let state = TaskState::new();
        let listed = state.query(&TaskQuery::new()).unwrap();
        let filter = TaskFilter::new()
            .with_label_selector("watched=true".parse().unwrap())
            .unwrap();
        let mut watch = state
            .watch(&filter, Some(listed.resource_version.as_str()))
            .unwrap();
        for index in 0..=WATCH_POLL_BUDGET {
            create(&state, &format!("irrelevant-{index}"));
        }

        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut watch).poll_next(&mut context),
            Poll::Pending
        ));
        assert_eq!(wake.0.load(Ordering::Relaxed), 1);

        state.close_watches();
        assert!(matches!(
            Pin::new(&mut watch).poll_next(&mut context),
            Poll::Ready(None)
        ));
    }

    #[tokio::test]
    async fn closing_state_watches_ends_the_stream() {
        let state = TaskState::new();
        let mut watch = state.watch(&TaskFilter::new(), Some("0")).unwrap();
        state.close_watches();
        assert!(watch.next().await.is_none());
    }
}
