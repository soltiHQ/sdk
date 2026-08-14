//! # Core configuration
//!
//! [`StateConfig`] controls Task admission and retained task, run, and
//! collection history.
//! [`ReconciliationConfig`] bounds runner construction.
//!
//! ```text
//! Taskvisor events
//!       ▼
//! TaskState
//!       ├── TaskRun history ──► run TTL and per-task cap
//!       ├── TaskRun changes ──► count and byte limits
//!       ├── terminal Tasks ───► task TTL
//!       ├── Task resources ───► retained task cap
//!       ├── Task manifests ───► aggregate byte budget
//!       ├── Task changes ─────► count and byte limits
//!       └── Task watches ─────► concurrent and buffered-byte limits
//! ```
//!
//! The supervisor owns the retention worker.
//! [`StateConfig::sweep_interval`] controls its cadence.

use std::{num::NonZeroUsize, time::Duration};

use thiserror::Error;
use tokio::sync::Semaphore;

const DEFAULT_MAX_RUNS_PER_TASK: usize = 256;
const DEFAULT_MAX_RETAINED_TASK_MANIFEST_BYTES: NonZeroUsize =
    NonZeroUsize::new(256 * 1024 * 1024).unwrap();
const DEFAULT_MAX_RETAINED_TASKS: NonZeroUsize = NonZeroUsize::new(1_024).unwrap();
const DEFAULT_MAX_CONCURRENT_TASK_WATCHES: NonZeroUsize = NonZeroUsize::new(256).unwrap();
const DEFAULT_MAX_TASK_WATCH_INITIAL_REPLAY_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024 * 1024).unwrap();
const DEFAULT_RUN_HISTORY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const DEFAULT_RUN_HISTORY_CAPACITY: usize = 4_096;
const DEFAULT_RUN_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_TASK_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_WATCH_HISTORY_BYTE_BUDGET: usize = 64 * 1024 * 1024;
const DEFAULT_WATCH_HISTORY_CAPACITY: usize = 4_096;
const MAX_WATCH_HISTORY_CAPACITY: usize = usize::MAX >> 1;
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

/// In-memory state admission and retention settings.
///
/// | Value                                                                    | Default   |
/// |--------------------------------------------------------------------------|-----------|
/// | [`run_ttl`](Self::run_ttl)                                               | 1 hour    |
/// | [`task_ttl`](Self::task_ttl)                                             | 1 hour    |
/// | [`sweep_interval`](Self::sweep_interval)                                 | 5 minutes |
/// | [`max_runs_per_task`](Self::max_runs_per_task)                           | 256       |
/// | [`max_retained_tasks`](Self::max_retained_tasks)                         | 1024      |
/// | [`max_retained_task_manifest_bytes`](Self::max_retained_task_manifest_bytes) | 256 MiB   |
/// | [`max_concurrent_task_watches`](Self::max_concurrent_task_watches)       | 256       |
/// | [`max_task_watch_initial_replay_bytes`](Self::max_task_watch_initial_replay_bytes) | 64 MiB |
/// | [`run_history_capacity`](Self::run_history_capacity)                     | 4096      |
/// | [`run_history_byte_budget`](Self::run_history_byte_budget)               | 64 MiB    |
/// | [`watch_history_capacity`](Self::watch_history_capacity)                 | 4096      |
/// | [`watch_history_byte_budget`](Self::watch_history_byte_budget)           | 64 MiB    |
///
/// Finished runs expire after `run_ttl`.
/// An unbound unfinished run can also expire after that age.
/// A terminal task expires after its run history is empty and `task_ttl` has elapsed.
/// A task with a runtime binding is not removed.
/// `max_retained_tasks` counts every stored task.
/// A full state rejects new names without eviction.
/// That count limit does not block changes to existing tasks.
/// `max_retained_task_manifest_bytes` counts canonical compact JSON bytes for
/// caller-owned manifests of every stored task. It excludes Task status and
/// TaskRun history. Positive manifest growth is rejected when it would exceed
/// this byte budget.
/// `max_concurrent_task_watches` counts admitted watch subscriptions.
/// `max_task_watch_initial_replay_bytes` counts compact Task JSON retained by
/// their initial and replay buffers. Live events already transferred to a
/// caller are outside that byte budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct StateConfig {
    /// Maximum age of an eligible retained run.
    run_ttl: Duration,
    /// Maximum age of an eligible terminal task.
    task_ttl: Duration,
    /// Interval between retention passes.
    sweep_interval: Duration,
    /// Maximum completed runs retained for one task.
    max_runs_per_task: usize,
    /// Maximum tasks retained by this state.
    max_retained_tasks: Option<NonZeroUsize>,
    /// Maximum caller-owned TaskManifest bytes retained by this state.
    max_retained_task_manifest_bytes: Option<NonZeroUsize>,
    /// Maximum concurrent Task watch subscriptions.
    max_concurrent_task_watches: Option<NonZeroUsize>,
    /// Maximum aggregate serialized bytes retained by Task watch initial and replay buffers.
    max_task_watch_initial_replay_bytes: Option<NonZeroUsize>,
    /// Maximum serialized bytes retained by the TaskRun journal.
    run_history_byte_budget: usize,
    /// Maximum TaskRun mutation batches retained by the journal.
    run_history_capacity: usize,
    /// Maximum serialized bytes retained by the Task change journal.
    watch_history_byte_budget: usize,
    /// Maximum Task changes retained by the journal.
    ///
    /// Core derives a smaller live ring when this value exceeds one, leaving
    /// retained journal headroom for lag recovery.
    watch_history_capacity: usize,
}

impl StateConfig {
    /// Creates the default state settings.
    pub const fn new() -> Self {
        Self {
            run_ttl: DEFAULT_RUN_TTL,
            task_ttl: DEFAULT_TASK_TTL,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            max_runs_per_task: DEFAULT_MAX_RUNS_PER_TASK,
            max_retained_tasks: Some(DEFAULT_MAX_RETAINED_TASKS),
            max_retained_task_manifest_bytes: Some(DEFAULT_MAX_RETAINED_TASK_MANIFEST_BYTES),
            max_concurrent_task_watches: Some(DEFAULT_MAX_CONCURRENT_TASK_WATCHES),
            max_task_watch_initial_replay_bytes: Some(DEFAULT_MAX_TASK_WATCH_INITIAL_REPLAY_BYTES),
            run_history_byte_budget: DEFAULT_RUN_HISTORY_BYTE_BUDGET,
            run_history_capacity: DEFAULT_RUN_HISTORY_CAPACITY,
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

    /// Returns the retained task limit.
    ///
    /// `None` disables this task-count limit.
    pub const fn max_retained_tasks(&self) -> Option<NonZeroUsize> {
        self.max_retained_tasks
    }

    /// Returns the aggregate retained TaskManifest byte budget.
    ///
    /// The budget counts canonical compact JSON bytes for caller-owned
    /// manifests. `None` disables this byte budget.
    pub const fn max_retained_task_manifest_bytes(&self) -> Option<NonZeroUsize> {
        self.max_retained_task_manifest_bytes
    }

    /// Returns the concurrent Task watch limit.
    ///
    /// `None` disables this watch-count limit.
    pub const fn max_concurrent_task_watches(&self) -> Option<NonZeroUsize> {
        self.max_concurrent_task_watches
    }

    /// Returns the aggregate Task watch initial and replay byte budget.
    ///
    /// The budget counts compact JSON bytes for Task objects retained by
    /// initial snapshots and replay buffers. `None` disables this byte budget.
    pub const fn max_task_watch_initial_replay_bytes(&self) -> Option<NonZeroUsize> {
        self.max_task_watch_initial_replay_bytes
    }

    /// Returns the retained TaskRun mutation-batch limit.
    pub const fn run_history_capacity(&self) -> usize {
        self.run_history_capacity
    }

    /// Returns the serialized TaskRun journal byte budget.
    pub const fn run_history_byte_budget(&self) -> usize {
        self.run_history_byte_budget
    }

    /// Returns the retained Task change limit.
    ///
    /// A value of one leaves no journal headroom for lag recovery.
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

    /// Sets the retained task limit.
    ///
    /// `None` disables this task-count limit.
    /// The limit belongs to one state store.
    pub const fn with_max_retained_tasks(
        mut self,
        max_retained_tasks: Option<NonZeroUsize>,
    ) -> Self {
        self.max_retained_tasks = max_retained_tasks;
        self
    }

    /// Sets the retained task limit from a raw value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_retained_tasks` is zero.
    pub const fn try_with_max_retained_tasks(
        self,
        max_retained_tasks: usize,
    ) -> Result<Self, ConfigError> {
        let Some(max_retained_tasks) = NonZeroUsize::new(max_retained_tasks) else {
            return Err(ConfigError::Zero {
                field: "max_retained_tasks",
            });
        };
        Ok(self.with_max_retained_tasks(Some(max_retained_tasks)))
    }

    /// Sets the aggregate retained TaskManifest byte budget.
    ///
    /// The budget counts canonical compact JSON bytes for caller-owned
    /// manifests. `None` disables this byte budget.
    pub const fn with_max_retained_task_manifest_bytes(
        mut self,
        max_retained_task_manifest_bytes: Option<NonZeroUsize>,
    ) -> Self {
        self.max_retained_task_manifest_bytes = max_retained_task_manifest_bytes;
        self
    }

    /// Sets the aggregate retained TaskManifest byte budget from a raw value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_retained_task_manifest_bytes` is zero.
    pub const fn try_with_max_retained_task_manifest_bytes(
        self,
        max_retained_task_manifest_bytes: usize,
    ) -> Result<Self, ConfigError> {
        let Some(max_retained_task_manifest_bytes) =
            NonZeroUsize::new(max_retained_task_manifest_bytes)
        else {
            return Err(ConfigError::Zero {
                field: "max_retained_task_manifest_bytes",
            });
        };
        Ok(self.with_max_retained_task_manifest_bytes(Some(max_retained_task_manifest_bytes)))
    }

    /// Sets the concurrent Task watch limit.
    ///
    /// `None` disables this watch-count limit.
    pub const fn with_max_concurrent_task_watches(
        mut self,
        max_concurrent_task_watches: Option<NonZeroUsize>,
    ) -> Self {
        self.max_concurrent_task_watches = max_concurrent_task_watches;
        self
    }

    /// Sets the concurrent Task watch limit from a raw value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_concurrent_task_watches` is zero.
    pub const fn try_with_max_concurrent_task_watches(
        self,
        max_concurrent_task_watches: usize,
    ) -> Result<Self, ConfigError> {
        let Some(max_concurrent_task_watches) = NonZeroUsize::new(max_concurrent_task_watches)
        else {
            return Err(ConfigError::Zero {
                field: "max_concurrent_task_watches",
            });
        };
        Ok(self.with_max_concurrent_task_watches(Some(max_concurrent_task_watches)))
    }

    /// Sets the aggregate Task watch initial and replay byte budget.
    ///
    /// `None` disables this byte budget.
    pub const fn with_max_task_watch_initial_replay_bytes(
        mut self,
        max_task_watch_initial_replay_bytes: Option<NonZeroUsize>,
    ) -> Self {
        self.max_task_watch_initial_replay_bytes = max_task_watch_initial_replay_bytes;
        self
    }

    /// Sets the aggregate Task watch initial and replay budget from a raw value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_task_watch_initial_replay_bytes` is zero.
    pub const fn try_with_max_task_watch_initial_replay_bytes(
        self,
        max_task_watch_initial_replay_bytes: usize,
    ) -> Result<Self, ConfigError> {
        let Some(max_task_watch_initial_replay_bytes) =
            NonZeroUsize::new(max_task_watch_initial_replay_bytes)
        else {
            return Err(ConfigError::Zero {
                field: "max_task_watch_initial_replay_bytes",
            });
        };
        Ok(
            self.with_max_task_watch_initial_replay_bytes(Some(
                max_task_watch_initial_replay_bytes,
            )),
        )
    }

    /// Sets the retained TaskRun mutation-batch limit.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `run_history_capacity` is zero.
    pub const fn try_with_run_history_capacity(
        mut self,
        run_history_capacity: usize,
    ) -> Result<Self, ConfigError> {
        if run_history_capacity == 0 {
            return Err(ConfigError::Zero {
                field: "run_history_capacity",
            });
        }
        self.run_history_capacity = run_history_capacity;
        Ok(self)
    }

    /// Sets the serialized TaskRun journal byte budget.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `run_history_byte_budget` is zero.
    pub const fn try_with_run_history_byte_budget(
        mut self,
        run_history_byte_budget: usize,
    ) -> Result<Self, ConfigError> {
        if run_history_byte_budget == 0 {
            return Err(ConfigError::Zero {
                field: "run_history_byte_budget",
            });
        }
        self.run_history_byte_budget = run_history_byte_budget;
        Ok(self)
    }

    /// Sets the retained Task change limit.
    ///
    /// Watches and list snapshots share this history.
    /// The structural maximum is `usize::MAX / 2`.
    /// State creation derives a power-of-two live broadcast ring below this
    /// limit when the limit exceeds one. A value of one remains valid but
    /// leaves no journal headroom for lag recovery.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `watch_history_capacity` is zero.
    /// Returns [`ConfigError::Exceeds`] when the value exceeds the supported
    /// structural limit.
    pub const fn try_with_watch_history_capacity(
        mut self,
        watch_history_capacity: usize,
    ) -> Result<Self, ConfigError> {
        if watch_history_capacity == 0 {
            return Err(ConfigError::Zero {
                field: "watch_history_capacity",
            });
        }
        if watch_history_capacity > MAX_WATCH_HISTORY_CAPACITY {
            return Err(ConfigError::Exceeds {
                field: "watch_history_capacity",
                limit: "watch_history_capacity_max",
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

    /// Returns the deadline for one routed runner build.
    ///
    /// The deadline starts before root admission. It includes nested catalog
    /// construction and admission waits.
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

    /// Sets the deadline for one routed runner build.
    ///
    /// The deadline starts before root admission. It includes nested catalog
    /// construction and admission waits.
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
        assert_eq!(config.max_retained_tasks(), NonZeroUsize::new(1_024));
        assert_eq!(
            config.max_retained_task_manifest_bytes(),
            NonZeroUsize::new(256 * 1024 * 1024)
        );
        assert_eq!(config.max_concurrent_task_watches(), NonZeroUsize::new(256));
        assert_eq!(
            config.max_task_watch_initial_replay_bytes(),
            NonZeroUsize::new(64 * 1024 * 1024)
        );
        assert_eq!(config.run_history_byte_budget(), 64 * 1024 * 1024);
        assert_eq!(config.run_history_capacity(), 4_096);
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
                    .try_with_max_retained_tasks(0)
                    .unwrap_err(),
                "max_retained_tasks",
            ),
            (
                StateConfig::new()
                    .try_with_max_retained_task_manifest_bytes(0)
                    .unwrap_err(),
                "max_retained_task_manifest_bytes",
            ),
            (
                StateConfig::new()
                    .try_with_max_concurrent_task_watches(0)
                    .unwrap_err(),
                "max_concurrent_task_watches",
            ),
            (
                StateConfig::new()
                    .try_with_max_task_watch_initial_replay_bytes(0)
                    .unwrap_err(),
                "max_task_watch_initial_replay_bytes",
            ),
            (
                StateConfig::new()
                    .try_with_run_history_capacity(0)
                    .unwrap_err(),
                "run_history_capacity",
            ),
            (
                StateConfig::new()
                    .try_with_run_history_byte_budget(0)
                    .unwrap_err(),
                "run_history_byte_budget",
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
    fn watch_history_capacity_enforces_the_structural_limit() {
        let maximum = StateConfig::new()
            .try_with_watch_history_capacity(MAX_WATCH_HISTORY_CAPACITY)
            .unwrap();
        assert_eq!(maximum.watch_history_capacity(), MAX_WATCH_HISTORY_CAPACITY);
        assert_eq!(
            StateConfig::new()
                .try_with_watch_history_capacity(MAX_WATCH_HISTORY_CAPACITY + 1)
                .unwrap_err(),
            ConfigError::Exceeds {
                field: "watch_history_capacity",
                limit: "watch_history_capacity_max",
            }
        );
    }

    #[test]
    fn zero_run_cap_is_valid() {
        let config = StateConfig::new().with_max_runs_per_task(0);
        assert_eq!(config.max_runs_per_task(), 0);
    }

    #[test]
    fn retained_task_limit_accepts_typed_raw_and_unbounded_values() {
        let typed = StateConfig::new().with_max_retained_tasks(NonZeroUsize::new(17));
        assert_eq!(typed.max_retained_tasks(), NonZeroUsize::new(17));

        let raw = StateConfig::new().try_with_max_retained_tasks(23).unwrap();
        assert_eq!(raw.max_retained_tasks(), NonZeroUsize::new(23));

        let unbounded = StateConfig::new().with_max_retained_tasks(None);
        assert_eq!(unbounded.max_retained_tasks(), None);
    }

    #[test]
    fn retained_task_manifest_budget_accepts_typed_raw_and_unbounded_values() {
        let typed = StateConfig::new().with_max_retained_task_manifest_bytes(NonZeroUsize::new(17));
        assert_eq!(
            typed.max_retained_task_manifest_bytes(),
            NonZeroUsize::new(17)
        );

        let raw = StateConfig::new()
            .try_with_max_retained_task_manifest_bytes(23)
            .unwrap();
        assert_eq!(
            raw.max_retained_task_manifest_bytes(),
            NonZeroUsize::new(23)
        );

        let unbounded = StateConfig::new().with_max_retained_task_manifest_bytes(None);
        assert_eq!(unbounded.max_retained_task_manifest_bytes(), None);
    }

    #[test]
    fn task_watch_limits_accept_typed_raw_and_unbounded_values() {
        let typed = StateConfig::new()
            .with_max_concurrent_task_watches(NonZeroUsize::new(17))
            .with_max_task_watch_initial_replay_bytes(NonZeroUsize::new(19));
        assert_eq!(typed.max_concurrent_task_watches(), NonZeroUsize::new(17));
        assert_eq!(
            typed.max_task_watch_initial_replay_bytes(),
            NonZeroUsize::new(19)
        );

        let raw = StateConfig::new()
            .try_with_max_concurrent_task_watches(23)
            .unwrap()
            .try_with_max_task_watch_initial_replay_bytes(29)
            .unwrap();
        assert_eq!(raw.max_concurrent_task_watches(), NonZeroUsize::new(23));
        assert_eq!(
            raw.max_task_watch_initial_replay_bytes(),
            NonZeroUsize::new(29)
        );

        let unbounded = StateConfig::new()
            .with_max_concurrent_task_watches(None)
            .with_max_task_watch_initial_replay_bytes(None);
        assert_eq!(unbounded.max_concurrent_task_watches(), None);
        assert_eq!(unbounded.max_task_watch_initial_replay_bytes(), None);
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
