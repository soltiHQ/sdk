//! # Core configuration
//!
//! [`StateConfig`] controls in-memory task, run, and collection history.
//! [`ReconciliationConfig`] bounds runner construction.
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
use tokio::sync::Semaphore;

const DEFAULT_MAX_RUNS_PER_TASK: usize = 256;
const DEFAULT_RUN_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_TASK_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_WATCH_HISTORY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const DEFAULT_WATCH_HISTORY_CAPACITY: usize = 4_096;
const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_CONCURRENT_BUILDS: usize = 32;
const DEFAULT_MAX_CONCURRENT_BUILDS_PER_RUNNER: usize = 8;

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
    /// A positive limit was below its structural minimum.
    #[error("{field} must be at least {minimum}")]
    #[non_exhaustive]
    BelowMinimum {
        /// Stable field name for the invalid value.
        field: &'static str,
        /// Smallest accepted value.
        minimum: usize,
    },
    /// One positive limit exceeded another limit.
    #[error("{field} must not exceed {limit}")]
    #[non_exhaustive]
    Exceeds {
        /// Stable field name for the invalid value.
        field: &'static str,
        /// Stable field name for the upper bound.
        limit: &'static str,
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

/// Runner-build admission and deadline settings.
///
/// Admission applies only to routed runner builds. One global slot covers an
/// outer build and its nested catalog builds. Every selected runner, including
/// a nested catalog runner, consumes its own per-runner slot. Embedded tasks
/// already carry a built [`taskvisor::TaskRef`] and do not consume build slots.
///
/// The defaults are SDK policy values, not a claim of a benchmark-derived
/// optimum. Applications can replace them with values measured for their runner
/// workloads and service objectives.
///
/// | Value                                                                      | Default    |
/// |----------------------------------------------------------------------------|------------|
/// | [`build_timeout`](Self::build_timeout)                                     | 30 seconds |
/// | [`max_concurrent_builds`](Self::max_concurrent_builds)                     | 32         |
/// | [`max_concurrent_builds_per_runner`](Self::max_concurrent_builds_per_runner) | 8          |
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct ReconciliationConfig {
    build_timeout: Duration,
    max_concurrent_builds: usize,
    max_concurrent_builds_per_runner: usize,
}

impl ReconciliationConfig {
    /// Creates the default reconciliation settings.
    pub const fn new() -> Self {
        Self {
            build_timeout: DEFAULT_BUILD_TIMEOUT,
            max_concurrent_builds: DEFAULT_MAX_CONCURRENT_BUILDS,
            max_concurrent_builds_per_runner: DEFAULT_MAX_CONCURRENT_BUILDS_PER_RUNNER,
        }
    }

    /// Returns the deadline for one admitted runner build.
    ///
    /// The deadline includes nested catalog construction and admission waits.
    pub const fn build_timeout(&self) -> Duration {
        self.build_timeout
    }

    /// Returns the concurrent outer runner-build limit.
    pub const fn max_concurrent_builds(&self) -> usize {
        self.max_concurrent_builds
    }

    /// Returns the concurrent build limit for one registered runner.
    ///
    /// The limit includes nested builds selected from a [`solti_runner::RunnerCatalog`].
    pub const fn max_concurrent_builds_per_runner(&self) -> usize {
        self.max_concurrent_builds_per_runner
    }

    /// Sets the deadline for one admitted runner build, including nested
    /// catalog construction and admission waits.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `build_timeout` is zero.
    pub const fn try_with_build_timeout(
        mut self,
        build_timeout: Duration,
    ) -> Result<Self, ConfigError> {
        if build_timeout.is_zero() {
            return Err(ConfigError::Zero {
                field: "build_timeout",
            });
        }
        self.build_timeout = build_timeout;
        Ok(self)
    }

    /// Sets the concurrent outer runner-build limit.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_concurrent_builds` is zero.
    /// Returns [`ConfigError::Exceeds`] when the value exceeds the Tokio
    /// semaphore limit.
    pub const fn try_with_max_concurrent_builds(
        mut self,
        max_concurrent_builds: usize,
    ) -> Result<Self, ConfigError> {
        if max_concurrent_builds == 0 {
            return Err(ConfigError::Zero {
                field: "max_concurrent_builds",
            });
        }
        if max_concurrent_builds > Semaphore::MAX_PERMITS {
            return Err(ConfigError::Exceeds {
                field: "max_concurrent_builds",
                limit: "semaphore_max_permits",
            });
        }
        self.max_concurrent_builds = max_concurrent_builds;
        Ok(self)
    }

    /// Sets the concurrent build limit for one registered runner, including
    /// nested catalog builds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_concurrent_builds_per_runner` is zero.
    /// Returns [`ConfigError::Exceeds`] when the value exceeds the Tokio
    /// semaphore limit.
    pub const fn try_with_max_concurrent_builds_per_runner(
        mut self,
        max_concurrent_builds_per_runner: usize,
    ) -> Result<Self, ConfigError> {
        if max_concurrent_builds_per_runner == 0 {
            return Err(ConfigError::Zero {
                field: "max_concurrent_builds_per_runner",
            });
        }
        if max_concurrent_builds_per_runner > Semaphore::MAX_PERMITS {
            return Err(ConfigError::Exceeds {
                field: "max_concurrent_builds_per_runner",
                limit: "semaphore_max_permits",
            });
        }
        self.max_concurrent_builds_per_runner = max_concurrent_builds_per_runner;
        Ok(self)
    }
}

impl Default for ReconciliationConfig {
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

    #[test]
    fn reconciliation_defaults_are_explicit() {
        const CONFIG: ReconciliationConfig = ReconciliationConfig::new();
        let config = ReconciliationConfig::default();

        assert_eq!(CONFIG, config);
        assert_eq!(config.build_timeout(), Duration::from_secs(30));
        assert_eq!(config.max_concurrent_builds(), 32);
        assert_eq!(config.max_concurrent_builds_per_runner(), 8);
    }

    #[test]
    fn reconciliation_limits_reject_zero() {
        for (actual, field) in [
            (
                ReconciliationConfig::new()
                    .try_with_build_timeout(Duration::ZERO)
                    .unwrap_err(),
                "build_timeout",
            ),
            (
                ReconciliationConfig::new()
                    .try_with_max_concurrent_builds(0)
                    .unwrap_err(),
                "max_concurrent_builds",
            ),
            (
                ReconciliationConfig::new()
                    .try_with_max_concurrent_builds_per_runner(0)
                    .unwrap_err(),
                "max_concurrent_builds_per_runner",
            ),
        ] {
            assert_eq!(actual, ConfigError::Zero { field });
        }
    }

    #[test]
    fn reconciliation_limits_reject_values_that_would_panic_semaphore_creation() {
        for (actual, field) in [
            (
                ReconciliationConfig::new()
                    .try_with_max_concurrent_builds(usize::MAX)
                    .unwrap_err(),
                "max_concurrent_builds",
            ),
            (
                ReconciliationConfig::new()
                    .try_with_max_concurrent_builds_per_runner(usize::MAX)
                    .unwrap_err(),
                "max_concurrent_builds_per_runner",
            ),
        ] {
            assert_eq!(
                actual,
                ConfigError::Exceeds {
                    field,
                    limit: "semaphore_max_permits",
                }
            );
        }

        assert!(
            ReconciliationConfig::new()
                .try_with_max_concurrent_builds(Semaphore::MAX_PERMITS)
                .is_ok()
        );
        assert!(
            ReconciliationConfig::new()
                .try_with_max_concurrent_builds_per_runner(Semaphore::MAX_PERMITS)
                .is_ok()
        );
    }
}
