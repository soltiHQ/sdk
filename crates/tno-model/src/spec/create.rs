use serde::{Deserialize, Serialize};

use crate::{
    LABEL_RUNNER_TAG, RunnerLabels,
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
    /// This selects which runner is responsible (subprocess process, wasm, container, etc.).
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
    /// Optional metadata for routing / scheduling / observability.
    ///
    /// Router uses key `runner-tag` (if present) to select a specific runner among those that support this `TaskKind`.
    #[serde(default, skip_serializing_if = "RunnerLabels::is_empty")]
    pub labels: RunnerLabels,
}

impl CreateSpec {
    /// Attach a runner tag label used by the router.
    ///
    /// The tag is stored under the [`LABEL_RUNNER_TAG`] key and later
    /// consumed by `RunnerRouter` to pick a specific runner instance.
    ///
    /// This is a builder-style helper:
    ///
    /// ```rust
    /// # use tno_model::{
    /// #   CreateSpec, RunnerLabels, TaskKind, RestartStrategy, BackoffStrategy,
    /// #   AdmissionStrategy, JitterStrategy, TaskEnv, Flag,
    /// # };
    /// let spec = CreateSpec {
    ///     slot: "demo".into(),
    ///     kind: TaskKind::Subprocess {
    ///         command: "ls".into(),
    ///         args: vec!["/tmp".into()],
    ///         env: TaskEnv::default(),
    ///         cwd: None,
    ///         fail_on_non_zero: Flag::enabled(),
    ///     },
    ///     timeout_ms: 5_000,
    ///     restart: RestartStrategy::Never,
    ///     backoff: BackoffStrategy {
    ///         jitter: JitterStrategy::None,
    ///         first_ms: 0,
    ///         max_ms: 0,
    ///         factor: 1.0,
    ///     },
    ///     admission: AdmissionStrategy::DropIfRunning,
    ///     labels: RunnerLabels::new(),
    /// }
    /// .with_runner_tag("runner-a");
    /// ```
    pub fn with_runner_tag(mut self, tag: impl Into<String>) -> Self {
        self.labels.insert(LABEL_RUNNER_TAG, tag);
        self
    }

    /// Return the runner tag label (if present).
    ///
    /// This is a thin wrapper over `labels.get(LABEL_RUNNER_TAG)` and is
    /// intended for consumers that perform routing / placement.
    pub fn runner_tag(&self) -> Option<&str> {
        self.labels.get(LABEL_RUNNER_TAG)
    }
}
