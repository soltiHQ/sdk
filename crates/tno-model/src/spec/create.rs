use serde::{Deserialize, Serialize};

use crate::{
    domain::{Slot, TimeoutMs},
    kind::TaskKind,
    strategy::{AdmissionStrategy, BackoffStrategy, RestartStrategy},
};

/// Declarative specification used when creating a new task.
///
/// `CreateSpec` describes *what* should be run and *how* it should be managed by the runtime.
///
/// Fields cover:
/// - logical grouping and concurrency control (`slot`, `admission`)
/// - execution backend (`kind`)
/// - lifecycle policies (`timeout_ms`, `restart`, `backoff`)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSpec {
    /// Logical slot name used for concurrency control.
    ///
    /// All tasks with the same slot share a single execution lane:
    /// admission rules decide what happens when a new task targets an already busy slot.
    pub slot: Slot,
    /// Execution backend used to run the task.
    ///
    /// This selects which runner is responsible (exec process, wasm, container, etc.).
    /// If no runner supports the given kind at runtime, task creation will fail.
    pub kind: TaskKind,
    /// Hard timeout for the task in milliseconds.
    ///
    /// Once this timeout is reached, the task is considered failed with timeout error.
    pub timeout_ms: TimeoutMs,
    /// Restart applied after a task completes or fails.
    ///
    /// Controls *whether* the task should be scheduled again (e.g. `OnFailure`, `Always`, `Never`).
    pub restart: RestartStrategy,
    /// Backoff configuration used between restart attempts.
    ///
    /// Defines *how long* to wait before the next run when the restart policy allows another attempt.
    pub backoff: BackoffStrategy,
    /// Admission for handling conflicts within the same slot.
    ///
    /// Controls what happens when a new task is submitted while a task in the same slot is already running (drop, replace, queue).
    pub admission: AdmissionStrategy,
}
