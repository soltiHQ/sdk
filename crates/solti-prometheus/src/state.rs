//! # Supervisor state collector.
//!
//! [`PrometheusCoreStateCollector`] exposes the current number of tasks per [`TaskPhase`] as `solti_core_tasks_by_phase{phase}`.

use std::sync::Mutex;

use prometheus::GaugeVec;
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use solti_core::TaskState;
use solti_model::TaskPhase;

use crate::register::gauge_vec_unregistered;

/// All phases we want to be represented as gauges, even at zero.
///
/// Future variants are aggregated under `phase="unknown"` until mapped here.
const ALL_PHASES: &[TaskPhase] = &[
    TaskPhase::Pending,
    TaskPhase::Running,
    TaskPhase::Succeeded,
    TaskPhase::Failed,
    TaskPhase::Timeout,
    TaskPhase::Canceled,
    TaskPhase::Exhausted,
];

#[inline]
fn phase_label(phase: TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Pending => "pending",
        TaskPhase::Running => "running",
        TaskPhase::Succeeded => "succeeded",
        TaskPhase::Failed => "failed",
        TaskPhase::Timeout => "timeout",
        TaskPhase::Canceled => "canceled",
        TaskPhase::Exhausted => "exhausted",
        _ => "unknown",
    }
}

/// Pull-based Prometheus collector for `solti_core_tasks_by_phase{phase}`.
///
/// Register once with a shared [`prometheus::Registry`] alongside the other Solti collectors.
/// Every known phase is emitted on each scrape. Empty phases return `0`.
/// Future phases are counted as `unknown`.
///
/// Counts are recomputed from [`TaskState`] on every scrape.
/// Unlike the event gauge `solti_taskvisor_attempts_in_flight`, this collector
/// recomputes its values from resource state.
/// The residual limitation is upstream: a `Running` count reflects the `AttemptStarting` events `TaskState` has observed.
/// A start dropped under bus lag is undercounted until the entry's phase next changes (bounded, not cumulative).
///
/// # Cost
///
/// `O(N)` per scrape where `N` is the current number of tasks in state:
/// [`TaskState::count_by_phase`] tallies the phases under a single read lock without cloning any task
/// (a clone would drag whole specs along, including script bodies).
///
/// With a typical scrape interval of 10-30s and a fleet of <10k tasks this is negligible.
///
/// # Example
///
/// ```rust
/// use solti_core::TaskState;
/// use solti_prometheus::{PrometheusCoreStateCollector, Registry};
///
/// # fn main() -> Result<(), prometheus::Error> {
/// let registry = Registry::new();
/// // In an agent, take the supervisor's shared state instead of a fresh one.
/// let state = TaskState::new();
/// let collector = PrometheusCoreStateCollector::new(state)?;
/// registry.register(Box::new(collector))?;
/// # Ok(())
/// # }
/// ```
pub struct PrometheusCoreStateCollector {
    state: TaskState,
    gauge: GaugeVec,
    collect_lock: Mutex<()>,
}

impl PrometheusCoreStateCollector {
    /// Create a new collector wired to `state`.
    ///
    /// # Example
    ///
    /// ```
    /// use prometheus::core::Collector;
    /// use solti_core::TaskState;
    /// use solti_prometheus::PrometheusCoreStateCollector;
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let state = TaskState::new();
    /// let collector = PrometheusCoreStateCollector::new(state)?;
    ///
    /// assert!(!collector.collect().is_empty());
    /// # Ok(()) }
    /// ```
    pub fn new(state: TaskState) -> Result<Self, prometheus::Error> {
        let gauge = gauge_vec_unregistered(
            "core",
            "tasks_by_phase",
            "Current number of tasks per phase (snapshot at scrape time)",
            &["phase"],
        )?;
        Ok(Self {
            state,
            gauge,
            collect_lock: Mutex::new(()),
        })
    }
}

impl std::fmt::Debug for PrometheusCoreStateCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusCoreStateCollector").finish()
    }
}

impl Collector for PrometheusCoreStateCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.gauge.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let _collect = self
            .collect_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counts = self.state.count_by_phase();
        for phase in ALL_PHASES {
            let count = counts.get(phase).copied().unwrap_or(0);
            self.gauge
                .with_label_values(&[phase_label(*phase)])
                .set(count as f64);
        }
        let unknown = counts
            .iter()
            .filter(|(phase, _)| !ALL_PHASES.contains(phase))
            .map(|(_, count)| count)
            .sum::<usize>();
        self.gauge
            .with_label_values(&["unknown"])
            .set(unknown as f64);
        self.gauge.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;
    use solti_model::{EmbeddedSpec, TaskId, TaskSpec, TaskWorkload};
    use std::sync::Arc;

    fn spec() -> TaskSpec {
        TaskSpec::builder(
            "slot",
            TaskWorkload::Embedded(EmbeddedSpec::new("prometheus-test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .expect("valid spec")
    }

    fn gauge_value(families: &[MetricFamily], name: &str, phase: &str) -> Option<f64> {
        families
            .iter()
            .find(|f| f.name() == name)?
            .get_metric()
            .iter()
            .find(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.name() == "phase" && l.value() == phase)
            })
            .map(|m| m.get_gauge().value())
    }

    #[test]
    fn collector_returns_zero_for_all_phases_when_empty() {
        let state = TaskState::new();
        let collector = PrometheusCoreStateCollector::new(state).unwrap();

        let families = collector.collect();
        for phase in [
            "pending",
            "running",
            "succeeded",
            "failed",
            "timeout",
            "canceled",
            "exhausted",
            "unknown",
        ] {
            assert_eq!(
                gauge_value(&families, "solti_core_tasks_by_phase", phase),
                Some(0.0),
                "phase {phase} must be zero on empty state",
            );
        }
    }

    #[test]
    fn collector_counts_pending_tasks() {
        let state = TaskState::new();
        state.seed_task(TaskId::new("t1").unwrap(), spec());
        state.seed_task(TaskId::new("t2").unwrap(), spec());
        state.seed_task(TaskId::new("t3").unwrap(), spec());

        let collector = PrometheusCoreStateCollector::new(state).unwrap();
        let families = collector.collect();

        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "pending"),
            Some(3.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "running"),
            Some(0.0)
        );
    }

    #[test]
    fn collector_reflects_transitions() {
        let state = TaskState::new();
        state.seed_task(TaskId::new("t1").unwrap(), spec());
        state.seed_task(TaskId::new("t2").unwrap(), spec());
        state.seed_starting(&TaskId::new("t1").unwrap());

        let collector = PrometheusCoreStateCollector::new(state.clone()).unwrap();
        let families = collector.collect();

        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "pending"),
            Some(1.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "running"),
            Some(1.0)
        );

        state.seed_finished(
            &TaskId::new("t1").unwrap(),
            TaskPhase::Succeeded,
            None,
            None,
        );
        let families = collector.collect();
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "running"),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "succeeded"),
            Some(1.0)
        );
    }

    #[test]
    fn collector_registers_into_registry_and_scrapes() {
        let registry = Arc::new(Registry::new());
        let state = TaskState::new();
        state.seed_task(TaskId::new("alpha").unwrap(), spec());
        state.seed_starting(&TaskId::new("alpha").unwrap());

        let collector = PrometheusCoreStateCollector::new(state).unwrap();
        registry.register(Box::new(collector)).unwrap();

        let families = registry.gather();
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "running"),
            Some(1.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_core_tasks_by_phase", "pending"),
            Some(0.0)
        );
    }
}
