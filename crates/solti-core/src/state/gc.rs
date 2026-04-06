use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{StateConfig, TaskState};

/// Logical slot name for the state GC task.
pub const GC_SLOT: &str = "solti-state-gc";

/// Per-attempt timeout in milliseconds (30 seconds).
const GC_TIMEOUT_MS: u64 = 30_000;

/// Initial backoff delay on failure (ms).
const BACKOFF_FIRST_MS: u64 = 5_000;

/// Maximum backoff delay on repeated failures (ms).
const BACKOFF_MAX_MS: u64 = 60_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Builds the state garbage collector task and its supervision specification.
///
/// The task periodically sweeps expired runs and terminal tasks from the
/// in-memory [`TaskState`] according to the TTL settings in [`StateConfig`].
///
/// ## Scheduling
///
/// | Scenario      | Delay           | Strategy                              |
/// |---------------|-----------------|---------------------------------------|
/// | Success       | `gc_interval`   | Periodic restart                      |
/// | Failure       | 5 s -> 60 s     | Exponential backoff with equal jitter |
/// | Duplicate     | Replaces        | [`AdmissionPolicy::Replace`]          |
///
/// ## Example
///
/// ```rust,ignore
/// use solti_core::state::{state_gc, StateConfig, TaskState};
///
/// let state = TaskState::new();
/// let config = StateConfig::default();
/// let (task, spec) = state_gc(state, config);
/// supervisor.submit_with_task(task, &spec).await?;
/// ```
pub fn state_gc(state: TaskState, config: StateConfig) -> (TaskRef, TaskSpec) {
    let gc_interval_ms = config.gc_interval.as_millis() as u64;

    let task: TaskRef = TaskFn::arc(GC_SLOT, move |ctx: CancellationToken| {
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
                "state gc completed"
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
    let spec = TaskSpec::builder(GC_SLOT, TaskKind::Embedded, GC_TIMEOUT_MS)
        .restart(RestartPolicy::periodic(gc_interval_ms))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .expect("state gc spec must be valid");

    (task, spec)
}
