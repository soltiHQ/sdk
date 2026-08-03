//! # State retention
//!
//! [`StateConfig`] controls in-memory task, run, and collection history.
//!
//! ```text
//! Taskvisor events
//!       ▼
//! TaskState
//!       ├── TaskRun history ──► run TTL and per-task cap
//!       ├── terminal Tasks ───► task TTL
//!       └── Task changes ─────► count and byte limits
//! ```
//!
//! The supervisor owns the retention worker.
//! [`StateConfig::sweep_interval`] controls its cadence.

use std::time::Duration;

use thiserror::Error;

const DEFAULT_MAX_RUNS_PER_TASK: usize = 256;
const DEFAULT_RUN_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_TASK_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_WATCH_HISTORY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const DEFAULT_WATCH_HISTORY_CAPACITY: usize = 4_096;

/// Error from checked core configuration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A positive value was zero.
    #[error("{field} must be greater than zero")]
    #[non_exhaustive]
    Zero {
        /// Stable field name.
        field: &'static str,
    },
}

/// In-memory retention settings.
///
/// | Value                                                                    | Default   |
/// |--------------------------------------------------------------------------|-----------|
/// | [`run_ttl`](Self::run_ttl)                                               | 1 hour    |
/// | [`task_ttl`](Self::task_ttl)                                             | 1 hour    |
/// | [`sweep_interval`](Self::sweep_interval)                                 | 5 minutes |
/// | [`max_runs_per_task`](Self::max_runs_per_task)                           | 256       |
/// | [`watch_history_capacity`](Self::watch_history_capacity)                 | 4096      |
/// | [`watch_history_byte_budget`](Self::watch_history_byte_budget)           | 64 MiB    |
///
/// Finished runs expire after `run_ttl`.
/// An unbound unfinished run can also expire after that age.
/// A terminal task expires after its run history is empty and `task_ttl` has elapsed.
/// A task with a runtime binding is not removed.
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
    /// Creates the default retention settings.
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

    /// Returns the run retention age.
    ///
    /// This also applies to unfinished runs without a runtime binding.
    pub const fn run_ttl(&self) -> Duration {
        self.run_ttl
    }

    /// Returns the terminal task retention age.
    ///
    /// The age starts at the internal terminal transition.
    /// Retention waits until run history is empty.
    pub const fn task_ttl(&self) -> Duration {
        self.task_ttl
    }

    /// Returns the retention worker interval.
    pub const fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }

    /// Returns the per-task completed run cap.
    ///
    /// Zero keeps active runs and removes every finished run.
    pub const fn max_runs_per_task(&self) -> usize {
        self.max_runs_per_task
    }

    /// Returns the retained Task change limit.
    pub const fn watch_history_capacity(&self) -> usize {
        self.watch_history_capacity
    }

    /// Returns the serialized Task payload budget for retained changes.
    pub const fn watch_history_byte_budget(&self) -> usize {
        self.watch_history_byte_budget
    }

    /// Sets the run retention age.
    ///
    /// Zero makes eligible runs removable on the next sweep.
    pub const fn with_run_ttl(mut self, run_ttl: Duration) -> Self {
        self.run_ttl = run_ttl;
        self
    }

    /// Sets the terminal task retention age.
    ///
    /// Zero makes eligible tasks removable on the next sweep.
    pub const fn with_task_ttl(mut self, task_ttl: Duration) -> Self {
        self.task_ttl = task_ttl;
        self
    }

    /// Sets the retention worker interval.
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

    /// Sets the per-task completed run cap.
    ///
    /// Zero disables completed run history.
    /// Active runs are never evicted by this cap.
    pub const fn with_max_runs_per_task(mut self, max_runs_per_task: usize) -> Self {
        self.max_runs_per_task = max_runs_per_task;
        self
    }

    /// Sets the retained Task change limit.
    ///
    /// Watches and list snapshots share this history.
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

    /// Sets the serialized Task payload budget for retained changes.
    ///
    /// Watches and list snapshots share this budget.
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
    fn checked_fields_reject_zero() {
        for (actual, field) in [
            (
                StateConfig::new()
                    .try_with_sweep_interval(Duration::ZERO)
                    .unwrap_err(),
                "sweep_interval",
            ),
            (
                StateConfig::new()
                    .try_with_watch_history_capacity(0)
                    .unwrap_err(),
                "watch_history_capacity",
            ),
            (
                StateConfig::new()
                    .try_with_watch_history_byte_budget(0)
                    .unwrap_err(),
                "watch_history_byte_budget",
            ),
        ] {
            assert_eq!(actual, ConfigError::Zero { field });
        }
    }

    #[test]
    fn zero_run_cap_is_valid() {
        let config = StateConfig::new().with_max_runs_per_task(0);
        assert_eq!(config.max_runs_per_task(), 0);
    }
}
