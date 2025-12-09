use async_trait::async_trait;
use taskvisor::{Event, EventKind, Subscribe};
use tracing::trace;

use super::TaskState;
use tno_model::{TaskId, TaskStatus};

/// Subscriber that updates TaskState from taskvisor events.
pub struct StateSubscriber {
    state: TaskState,
}

impl StateSubscriber {
    /// Create a new state subscriber.
    pub fn new(state: TaskState) -> Self {
        Self { state }
    }

    /// Extract TaskId from event.
    fn task_id_from_event(event: &Event) -> Option<TaskId> {
        event.task.as_ref().map(|s| TaskId::from(&**s))
    }

    /// Extract slot from TaskId (format: runner-slot-seq).
    fn slot_from_task_id(id: &TaskId) -> String {
        let parts: Vec<&str> = id.as_str().split('-').collect();
        if parts.len() >= 2 {
            parts[1].to_string()
        } else {
            "unknown".to_string()
        }
    }
}

#[async_trait]
impl Subscribe for StateSubscriber {
    async fn on_event(&self, event: &Event) {
        let Some(task_id) = Self::task_id_from_event(event) else {
            return;
        };

        match event.kind {
            EventKind::TaskAdded => {
                trace!(task = %task_id, "task added event received (already in state)");
            }
            EventKind::TaskStarting => {
                trace!(task = %task_id, "task starting");
                self.state.increment_attempt(&task_id);
                self.state
                    .update_status(&task_id, TaskStatus::Running, None);
            }
            EventKind::TaskStopped => {
                trace!(task = %task_id, "task stopped (success)");
                self.state
                    .update_status(&task_id, TaskStatus::Succeeded, None);
            }
            EventKind::TaskFailed => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                trace!(task = %task_id, reason = %reason, "task failed");
                self.state
                    .update_status(&task_id, TaskStatus::Failed, Some(reason));
            }
            EventKind::TimeoutHit => {
                trace!(task = %task_id, "task timeout");
                self.state.update_status(
                    &task_id,
                    TaskStatus::Timeout,
                    Some("timeout".to_string()),
                );
            }
            EventKind::ActorExhausted => {
                let reason = event
                    .reason
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "exhausted".to_string());
                trace!(task = %task_id, "task exhausted");
                self.state
                    .update_status(&task_id, TaskStatus::Exhausted, Some(reason));
            }
            EventKind::TaskRemoved => {
                trace!(task = %task_id, "task removed from state");
                self.state.remove_task(&task_id);
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
