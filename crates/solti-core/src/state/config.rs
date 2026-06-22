//! # State configuration.
//!
//! [`StateConfig`] controls TTLs and sweep interval for [`TaskState`](super::TaskState).

use std::time::Duration;

/// Default per-task run-history cap (see [`StateConfig::max_runs_per_task`]).
pub(crate) const DEFAULT_MAX_RUNS_PER_TASK: usize = 256;

/// Configuration for in-memory state retention and periodic sweep.
///
/// | Parameter           | Controls                                                      | Default    |
/// |---------------------|---------------------------------------------------------------|------------|
/// | `run_ttl`           | How long finished runs are retained before sweep removes them | 1 hour     |
/// | `task_ttl`          | How long terminal tasks (no active runs) are retained         | 1 hour     |
/// | `sweep_interval`    | How often the sweep runs                                      | 5 minutes  |
/// | `max_runs_per_task` | Hard cap on retained runs per task (oldest finished evicted)  | 256        |
///
/// ## Also
///
/// - [`TaskState`](crate::TaskState)'s crate-internal sweep consumes the TTL settings.
/// - [`SupervisorApi::new`](crate::SupervisorApi::new) accepts this config and auto-starts the sweep task.
#[derive(Debug, Clone)]
pub struct StateConfig {
    /// How long to keep completed runs before sweep removes them.
    pub run_ttl: Duration,
    /// How long to keep tasks in a terminal phase (with no active runs) before sweep removes them.
    pub task_ttl: Duration,
    /// How often the sweep embedded task runs.
    pub sweep_interval: Duration,
    /// Hard upper bound on the number of runs retained per task.
    /// When a new attempt starts and the history would exceed this, the oldest **finished** runs are evicted (the in-flight tail is never dropped).
    /// This bounds memory for fast-restarting tasks *between* sweeps; `list_task_runs` therefore makes no completeness guarantee for very chatty tasks.
    pub max_runs_per_task: usize,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            run_ttl: Duration::from_secs(3_600),
            task_ttl: Duration::from_secs(3_600),
            sweep_interval: Duration::from_secs(300),
            max_runs_per_task: DEFAULT_MAX_RUNS_PER_TASK,
        }
    }
}
