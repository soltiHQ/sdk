//! # State configuration.
//!
//! [`StateConfig`] controls TTLs and GC sweep interval for [`TaskState`](super::TaskState).

use std::time::Duration;

/// Configuration for in-memory state retention and garbage collection.
///
/// | Parameter       | Controls                                                      | Default     |
/// |-----------------|---------------------------------------------------------------|-------------|
/// | `run_ttl`       | How long finished runs are retained before GC removes them    | 1 hour      |
/// | `task_ttl`      | How long terminal tasks (no active runs) are retained         | 1 hour      |
/// | `gc_interval`   | How often the GC sweep runs                                   | 5 minutes   |
///
/// ## Also
///
/// - `TaskState::sweep` consumes these TTL settings.
/// - [`SupervisorApi::enable_gc`](crate::SupervisorApi::enable_gc) passes config to the GC task.
#[derive(Debug, Clone)]
pub struct StateConfig {
    /// How long to keep completed runs before garbage collection.
    pub run_ttl: Duration,
    /// How long to keep tasks in a terminal phase (with no active runs) before garbage collection.
    pub task_ttl: Duration,
    /// How often the GC embedded task runs.
    pub gc_interval: Duration,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            run_ttl: Duration::from_secs(3_600),
            task_ttl: Duration::from_secs(3_600),
            gc_interval: Duration::from_secs(300),
        }
    }
}
