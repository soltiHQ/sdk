//! # State configuration.
//!
//! [`StateConfig`] controls TTLs and sweep interval for [`TaskState`](super::TaskState).

use std::time::Duration;

/// Configuration for in-memory state retention and periodic sweep.
///
/// | Parameter        | Controls                                                    | Default     |
/// |------------------|-------------------------------------------------------------|-------------|
/// | `run_ttl`        | How long finished runs are retained before sweep removes them | 1 hour    |
/// | `task_ttl`       | How long terminal tasks (no active runs) are retained         | 1 hour    |
/// | `sweep_interval` | How often the sweep runs                                      | 5 minutes |
///
/// ## Also
///
/// - `TaskState::sweep` consumes these TTL settings.
/// - [`SupervisorApi::new`](crate::SupervisorApi::new) accepts this config and auto-starts the sweep task.
#[derive(Debug, Clone)]
pub struct StateConfig {
    /// How long to keep completed runs before sweep removes them.
    pub run_ttl: Duration,
    /// How long to keep tasks in a terminal phase (with no active runs) before sweep removes them.
    pub task_ttl: Duration,
    /// How often the sweep embedded task runs.
    pub sweep_interval: Duration,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            run_ttl: Duration::from_secs(3_600),
            task_ttl: Duration::from_secs(3_600),
            sweep_interval: Duration::from_secs(300),
        }
    }
}
