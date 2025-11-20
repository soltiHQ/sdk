use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::error::{ModelError, ModelResult};

/// Defines how the controller admits a new task into a slot.
///
/// A slot may only run one task at a time.
/// When a new task arrives, the controller applies the selected strategy to determine what to do if the slot is already occupied.
///
/// Strategies:
/// - `DropIfRunning`: Ignore the new task and return success without scheduling it.
/// - `Replace`: Cancel the currently running task and run the new one instead.
/// - `Queue`: Enqueue the new task and run it once the slot becomes free.
///
/// This value is typically provided in task creation requests or in controller configuration.
/// How a strategy is enforced at runtime depends on the runner and the supervisor admission logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdmissionStrategy {
    /// If the slot already has a running task, ignore the new one.
    /// The caller receives success, but the new task is not executed.
    DropIfRunning,
    /// Cancel the currently running task in the slot and replace it with the newly submitted task.
    Replace,
    /// Enqueue the new task to be executed after the current one completes.
    Queue,
}

impl Default for AdmissionStrategy {
    fn default() -> Self {
        AdmissionStrategy::DropIfRunning
    }
}

impl FromStr for AdmissionStrategy {
    type Err = ModelError;
    fn from_str(s: &str) -> ModelResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "drop-if-running" | "drop" => Ok(AdmissionStrategy::DropIfRunning),
            "queue" | "add" | "new" | "" => Ok(AdmissionStrategy::Queue),
            "replace" => Ok(AdmissionStrategy::Replace),
            other => Err(ModelError::UnknownAdmission(other.to_string())),
        }
    }
}
