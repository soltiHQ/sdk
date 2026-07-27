//! Core configuration.

use std::time::Duration;

use thiserror::Error;

const DEFAULT_MAX_RUNS_PER_TASK: usize = 256;
const DEFAULT_RUN_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_TASK_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_WATCH_HISTORY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const DEFAULT_WATCH_HISTORY_CAPACITY: usize = 4_096;

/// Error from a checked core configuration setter.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A value that must be positive was zero.
    #[error("{field} must be greater than zero")]
    #[non_exhaustive]
    Zero {
        /// Stable configuration field name.
        field: &'static str,
    },
}

/// In-memory state retention settings.
///
/// Finished runs are removed after [`run_ttl`](Self::run_ttl). A terminal task
/// is removed after its run history is empty and [`task_ttl`](Self::task_ttl)
/// has elapsed. The internal retention worker runs at
/// [`sweep_interval`](Self::sweep_interval).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct StateConfig {
    run_ttl: Duration,
    task_ttl: Duration,
    sweep_interval: Duration,
    max_runs_per_task: usize,
    watch_history_byte_budget: usize,
    watch_history_capacity: usize,
}

impl StateConfig {
    /// Create the default retention configuration.
    pub const fn new() -> Self {
        Self {
            run_ttl: DEFAULT_RUN_TTL,
            task_ttl: DEFAULT_TASK_TTL,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            max_runs_per_task: DEFAULT_MAX_RUNS_PER_TASK,
            watch_history_byte_budget: DEFAULT_WATCH_HISTORY_BYTE_BUDGET,
            watch_history_capacity: DEFAULT_WATCH_HISTORY_CAPACITY,
        }
    }

    /// Return how long finished runs are retained.
    pub const fn run_ttl(&self) -> Duration {
        self.run_ttl
    }

    /// Return how long terminal tasks are retained after their run history is empty.
    pub const fn task_ttl(&self) -> Duration {
        self.task_ttl
    }

    /// Return how often the retention worker runs.
    pub const fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }

    /// Return the per-task run-history cap.
    ///
    /// Zero keeps active runs and removes every finished run.
    pub const fn max_runs_per_task(&self) -> usize {
        self.max_runs_per_task
    }

    /// Return the number of Task changes retained for collection replay.
    pub const fn watch_history_capacity(&self) -> usize {
        self.watch_history_capacity
    }

    /// Return the serialized Task payload budget for retained collection changes.
    pub const fn watch_history_byte_budget(&self) -> usize {
        self.watch_history_byte_budget
    }

    /// Set how long finished runs are retained.
    pub const fn with_run_ttl(mut self, run_ttl: Duration) -> Self {
        self.run_ttl = run_ttl;
        self
    }

    /// Set how long terminal tasks are retained after their run history is empty.
    pub const fn with_task_ttl(mut self, task_ttl: Duration) -> Self {
        self.task_ttl = task_ttl;
        self
    }

    /// Set how often the retention worker runs.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `sweep_interval` is zero.
    pub const fn try_with_sweep_interval(
        mut self,
        sweep_interval: Duration,
    ) -> Result<Self, ConfigError> {
        if sweep_interval.is_zero() {
            return Err(ConfigError::Zero {
                field: "sweep_interval",
            });
        }
        self.sweep_interval = sweep_interval;
        Ok(self)
    }

    /// Set the per-task run-history cap.
    ///
    /// Zero disables completed run history. Active runs are never evicted by
    /// this cap.
    pub const fn with_max_runs_per_task(mut self, max_runs_per_task: usize) -> Self {
        self.max_runs_per_task = max_runs_per_task;
        self
    }

    /// Set the number of Task changes retained for watch replay and list snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `watch_history_capacity` is zero.
    pub const fn try_with_watch_history_capacity(
        mut self,
        watch_history_capacity: usize,
    ) -> Result<Self, ConfigError> {
        if watch_history_capacity == 0 {
            return Err(ConfigError::Zero {
                field: "watch_history_capacity",
            });
        }
        self.watch_history_capacity = watch_history_capacity;
        Ok(self)
    }

    /// Set the serialized Task payload budget for watch replay and list snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `watch_history_byte_budget` is zero.
    pub const fn try_with_watch_history_byte_budget(
        mut self,
        watch_history_byte_budget: usize,
    ) -> Result<Self, ConfigError> {
        if watch_history_byte_budget == 0 {
            return Err(ConfigError::Zero {
                field: "watch_history_byte_budget",
            });
        }
        self.watch_history_byte_budget = watch_history_byte_budget;
        Ok(self)
    }
}

impl Default for StateConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_contract_is_explicit() {
        const CONFIG: StateConfig = StateConfig::new();
        let config = StateConfig::default();

        assert_eq!(CONFIG, config);
        assert_eq!(config.run_ttl(), Duration::from_secs(3_600));
        assert_eq!(config.task_ttl(), Duration::from_secs(3_600));
        assert_eq!(config.sweep_interval(), Duration::from_secs(300));
        assert_eq!(config.max_runs_per_task(), 256);
        assert_eq!(config.watch_history_byte_budget(), 64 * 1024 * 1024);
        assert_eq!(config.watch_history_capacity(), 4_096);
    }

    #[test]
    fn zero_sweep_interval_is_rejected() {
        assert_eq!(
            StateConfig::new()
                .try_with_sweep_interval(Duration::ZERO)
                .unwrap_err(),
            ConfigError::Zero {
                field: "sweep_interval"
            }
        );
    }

    #[test]
    fn zero_run_cap_is_valid() {
        let config = StateConfig::new().with_max_runs_per_task(0);
        assert_eq!(config.max_runs_per_task(), 0);
    }

    #[test]
    fn zero_watch_history_capacity_is_rejected() {
        assert_eq!(
            StateConfig::new()
                .try_with_watch_history_capacity(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "watch_history_capacity"
            }
        );
    }

    #[test]
    fn zero_watch_history_byte_budget_is_rejected() {
        assert_eq!(
            StateConfig::new()
                .try_with_watch_history_byte_budget(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "watch_history_byte_budget"
            }
        );
    }
}
