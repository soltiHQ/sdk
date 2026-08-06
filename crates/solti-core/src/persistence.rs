//! # Persistence hooks
//!
//! These hooks let an agent forward task state and output to external storage.
//!
//! Hook callbacks run synchronously on the publishing path.
//! Implementations must return quickly, must not call back into core, and should
//! normally forward cloned events to an application-owned worker.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use solti_model::{OutputEvent, Task, TaskId, TaskRun, Uid};

/// One committed change to task or run state.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TaskStateEvent {
    /// One task resource was created, changed, or deleted.
    ///
    /// Create has no previous value.
    /// Delete has no current value.
    /// Task snapshots use [`Arc`] so cloned events remain cheap to forward.
    TaskChanged {
        /// Resource version assigned to this change.
        resource_version: String,
        /// Resource value before the change.
        previous: Option<Arc<Task>>,
        /// Resource value after the change.
        current: Option<Arc<Task>>,
    },
    /// One run was created or changed.
    ///
    /// Run retention does not publish delete events.
    /// This hook is a lifecycle journal, not a mirror of the in-memory retention window.
    RunChanged {
        /// Stable task name that owns the run.
        task: TaskId,
        /// Exact task incarnation that owns the run.
        task_uid: Uid,
        /// Current run value after the change.
        run: TaskRun,
    },
}

/// Synchronous receiver of committed task state changes.
///
/// The callback is isolated from panics, but it must not block or call back into core.
/// A database integration should forward the cloned event to its own worker.
pub trait TaskStateSink: Send + Sync + 'static {
    /// Receives one committed state event.
    fn on_event(&self, event: &TaskStateEvent);
}

/// Shared task state sink.
pub type TaskStateSinkHandle = Arc<dyn TaskStateSink>;

/// One output event with its task name and exact resource UID.
#[derive(Clone, Debug)]
pub struct TaskOutputEvent {
    task: TaskId,
    task_uid: Uid,
    event: OutputEvent,
}

impl TaskOutputEvent {
    pub(crate) fn new(task: TaskId, task_uid: Uid, event: OutputEvent) -> Self {
        Self {
            task,
            task_uid,
            event,
        }
    }

    /// Returns the task name that produced the output.
    pub fn task(&self) -> &TaskId {
        &self.task
    }

    /// Returns the exact task incarnation that produced the output.
    pub fn task_uid(&self) -> &Uid {
        &self.task_uid
    }

    /// Returns the original output event.
    pub fn event(&self) -> &OutputEvent {
        &self.event
    }

    /// Splits the wrapper into its task name, UID, and output event.
    pub fn into_parts(self) -> (TaskId, Uid, OutputEvent) {
        (self.task, self.task_uid, self.event)
    }
}

/// Synchronous receiver of task output events.
///
/// The sink receives output from the first event because it is installed before runners start.
/// The callback is isolated from panics, but it must not block runner execution.
/// A database integration should forward the cloned event to its own worker.
pub trait TaskOutputSink: Send + Sync + 'static {
    /// Receives one task output event.
    fn on_event(&self, event: &TaskOutputEvent);
}

/// Shared task output sink.
pub type TaskOutputSinkHandle = Arc<dyn TaskOutputSink>;

pub(crate) struct PersistenceSinks {
    pub(crate) state: Option<TaskStateSinkHandle>,
    pub(crate) output: Option<TaskOutputSinkHandle>,
}

pub(crate) fn publish_state_event(sink: Option<&TaskStateSinkHandle>, event: TaskStateEvent) {
    let Some(sink) = sink else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| sink.on_event(&event))).is_err() {
        tracing::warn!("task state persistence sink panicked; event was dropped");
    }
}

pub(crate) fn publish_output_event(sink: Option<&TaskOutputSinkHandle>, event: TaskOutputEvent) {
    let Some(sink) = sink else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| sink.on_event(&event))).is_err() {
        tracing::warn!("task output persistence sink panicked; event was dropped");
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use solti_model::{OutputEvent, TaskId};

    use super::*;

    struct PanickingStateSink;

    impl TaskStateSink for PanickingStateSink {
        fn on_event(&self, _event: &TaskStateEvent) {
            panic!("state sink panic");
        }
    }

    struct PanickingOutputSink;

    impl TaskOutputSink for PanickingOutputSink {
        fn on_event(&self, _event: &TaskOutputEvent) {
            panic!("output sink panic");
        }
    }

    #[test]
    fn sink_panics_are_isolated() {
        let state: TaskStateSinkHandle = Arc::new(PanickingStateSink);
        publish_state_event(
            Some(&state),
            TaskStateEvent::TaskChanged {
                resource_version: "test:1".to_string(),
                previous: None,
                current: None,
            },
        );

        let output: TaskOutputSinkHandle = Arc::new(PanickingOutputSink);
        publish_output_event(
            Some(&output),
            TaskOutputEvent::new(
                TaskId::new("panic-test").unwrap(),
                Uid::new("panic-test-uid").unwrap(),
                OutputEvent::RunStarted {
                    generation: 1,
                    attempt: 1,
                    started_at: SystemTime::UNIX_EPOCH,
                },
            ),
        );
    }
}
