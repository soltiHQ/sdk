//! # State sweep.
//!
//! [`state_sweep`] builds an embedded periodic task that sweeps expired runs and terminal tasks from [`TaskState`](super::TaskState).

use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy, TaskManifest,
    TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tracing::debug;

use super::{StateConfig, TaskState};

/// Reserved resource name for the state sweep task.
pub(crate) const SWEEP_NAME: &str = "solti-state-sweep";

/// Reserved logical slot for the state sweep task.
pub(crate) const SWEEP_SLOT: &str = "solti-state-sweep";

/// Per-attempt timeout in milliseconds (30 seconds).
const SWEEP_TIMEOUT_MS: u64 = 30_000;

/// Initial backoff delay on failure (ms).
const BACKOFF_FIRST_MS: u64 = 5_000;

/// Maximum backoff delay on repeated failures (ms).
const BACKOFF_MAX_MS: u64 = 60_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Builds the state sweep task and its supervision specification.
///
/// The task periodically sweeps expired runs and terminal tasks from the
/// in-memory [`TaskState`] according to the TTL settings in [`StateConfig`].
///
/// ## Scheduling
///
/// | Scenario      | Delay              | Strategy                              |
/// |---------------|--------------------|---------------------------------------|
/// | Success       | `sweep_interval`   | Periodic restart                      |
/// | Failure       | 5 s -> 60 s        | Exponential backoff with equal jitter |
/// | Duplicate     | Replaces           | [`AdmissionPolicy::Replace`]          |
///
/// ## Example
///
/// ```text
/// let state = TaskState::new();
/// let config = StateConfig::default();
/// let (task_ref, task) = state_sweep(state, config);
/// supervisor.create_with_task(task, task_ref).await?;
/// ```
pub(crate) fn state_sweep(state: TaskState, config: StateConfig) -> (TaskRef, TaskManifest) {
    let sweep_interval_ms = config.sweep_interval.as_millis() as u64;

    let task_ref: TaskRef = TaskFn::arc(SWEEP_NAME, move |ctx: TaskContext| {
        let state = state.clone();
        let config = config.clone();

        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }

            let (runs, tasks) = state.sweep(&config);
            debug!(
                runs_removed = runs,
                tasks_removed = tasks,
                "state sweep completed"
            );

            Ok(())
        }
    });

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: BACKOFF_FIRST_MS,
        max_ms: BACKOFF_MAX_MS,
        factor: BACKOFF_FACTOR,
    };
    let workload = TaskWorkload::Embedded(
        EmbeddedSpec::new(env!("CARGO_PKG_VERSION")).expect("package version must be non-empty"),
    );
    let spec = TaskSpec::builder(SWEEP_SLOT, workload, SWEEP_TIMEOUT_MS)
        .restart(RestartPolicy::periodic(sweep_interval_ms))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .expect("state sweep spec must be valid");

    let manifest =
        TaskManifest::new(SWEEP_NAME, spec).expect("state sweep Task manifest must be valid");
    (task_ref, manifest)
}
