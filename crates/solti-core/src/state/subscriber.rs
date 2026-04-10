//! # State event subscriber.
//!
//! [`StateSubscriber`] implements [`Subscribe`](taskvisor::Subscribe) to wire
//! taskvisor lifecycle events into [`TaskState`](super::TaskState) mutations.

use std::sync::Arc;

use taskvisor::{Event, EventKind, Subscribe};
use tracing::{trace, warn};

use super::TaskState;
use solti_model::{TaskId, TaskPhase};

/// Subscriber that updates TaskState from taskvisor events.
///
/// ## Also
///
/// - [`TaskState`](super::TaskState) storage mutated by this subscriber.
/// - [`SupervisorApi::new`](crate::SupervisorApi::new) auto-registers this subscriber.
pub struct StateSubscriber {
    state: TaskState,
}

impl StateSubscriber {
    /// Create a new state subscriber.
    pub fn new(state: TaskState) -> Self {
        Self { state }
    }

    /// Extract TaskId from event, reusing the existing `Arc<str>` allocation.
    fn task_id_from_event(event: &Event) -> Option<TaskId> {
        event.task.as_ref().map(|s| TaskId::from(Arc::clone(s)))
    }
}

impl Subscribe for StateSubscriber {
    fn on_event(&self, event: &Event) {
        let Some(task_id) = Self::task_id_from_event(event) else {
            return;
        };

        match event.kind {
            EventKind::TaskAdded => {
                trace!(task = %task_id, "task added event received (already in state)");
            }
            EventKind::TaskStarting => {
                trace!(task = %task_id, "task starting");
                if self.state.transition_starting(&task_id).is_none() {
                    warn!(task = %task_id, "TaskStarting event for unknown task");
                }
            }
            EventKind::TaskStopped => {
                trace!(task = %task_id, "task stopped (success)");
                if !self
                    .state
                    .transition_finished(&task_id, TaskPhase::Succeeded, None, None)
                {
                    warn!(task = %task_id, "TaskStopped event for unknown task");
                }
            }
            EventKind::TaskFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                trace!(task = %task_id, reason = %reason, "task failed");
                if !self
                    .state
                    .transition_finished(&task_id, TaskPhase::Failed, Some(reason), None)
                {
                    warn!(task = %task_id, "TaskFailed event for unknown task");
                }
            }
            EventKind::TimeoutHit => {
                trace!(task = %task_id, "task timeout");
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Timeout,
                    Some("timeout".to_string()),
                    None,
                ) {
                    warn!(task = %task_id, "TimeoutHit event for unknown task");
                }
            }
            EventKind::ActorExhausted => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "exhausted".to_string());
                trace!(task = %task_id, "task exhausted");
                if !self.state.transition_finished(
                    &task_id,
                    TaskPhase::Exhausted,
                    Some(reason),
                    None,
                ) {
                    warn!(task = %task_id, "ActorExhausted event for unknown task");
                }
            }
            EventKind::TaskRemoved => {
                trace!(task = %task_id, "task removed from state");
                self.state.unregister_task(&task_id);
            }
            _ => {}
        }
    }

    fn name(&self) -> &'static str {
        "state-subscriber"
    }

    fn queue_capacity(&self) -> usize {
        2048
    }
}
