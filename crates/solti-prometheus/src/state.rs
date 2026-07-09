//! # Supervisor state collector.
//!
//! [`PrometheusStateCollector`] exposes the current number of tasks per [`TaskPhase`] as `solti_sv_tasks_by_phase{phase}`.

use prometheus::GaugeVec;
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use solti_core::TaskState;
use solti_model::TaskPhase;

use crate::register::Sub;

/// All phases we want to be represented as gauges, even at zero.
///
/// Kept in code (not derived) because [`TaskPhase`] is `#[non_exhaustive]`: adding a variant upstream should be a conscious decision here too.
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

/// Pull-based Prometheus collector for `solti_sv_tasks_by_phase{phase}`.
///
/// Register once with a shared [`prometheus::Registry`] alongside the other Solti collectors.
/// On each scrape, all phases from [`TaskPhase`] are emitted; empty phases return `0` so dashboards can rely on a stable label set.
///
/// Counts are recomputed from [`TaskState`] on every scrape.
/// Unlike the event-gauge `solti_sv_tasks_in_flight`, this collector self-corrects and never accrues drift.
/// The residual limitation is upstream: a `Running` count reflects the `TaskStarting` events `TaskState` has observed.
/// A start dropped under bus lag is undercounted until the entry's phase next changes (bounded, not cumulative).
///
/// ## Cost
///
/// `O(N)` per scrape where `N` is the current number of tasks in state:
/// [`TaskState::count_by_phase`] tallies the phases under a single read lock without cloning any task
/// (a clone would drag whole specs along, including script bodies).
///
/// With a typical scrape interval of 10-30s and a fleet of <10k tasks this is negligible.
///
/// ## Example
///
/// ```rust
/// use solti_core::TaskState;
/// use solti_prometheus::{PrometheusStateCollector, Registry};
///
/// # fn main() -> Result<(), prometheus::Error> {
/// let registry = Registry::new();
/// // In an agent, take the supervisor's shared state instead of a fresh one.
/// let state = TaskState::new();
/// let collector = PrometheusStateCollector::new(state)?;
/// registry.register(Box::new(collector))?;
/// # Ok(())
/// # }
/// ```
pub struct PrometheusStateCollector {
    state: TaskState,
    gauge: GaugeVec,
}

impl PrometheusStateCollector {
    /// Create a new collector wired to `state`.
    ///
    /// ## Example
    ///
    /// ```
    /// use prometheus::core::Collector;
    /// use solti_core::TaskState;
    /// use solti_prometheus::PrometheusStateCollector;
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let state = TaskState::new();
    /// let collector = PrometheusStateCollector::new(state)?;
    ///
    /// assert!(!collector.collect().is_empty());
    /// # Ok(()) }
    /// ```
    pub fn new(state: TaskState) -> Result<Self, prometheus::Error> {
        let gauge = Sub::gauge_vec_unregistered(
            "sv",
            "tasks_by_phase",
            "Current number of tasks per phase (snapshot at scrape time)",
            &["phase"],
        )?;
        Ok(Self { state, gauge })
    }
}

impl std::fmt::Debug for PrometheusStateCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusStateCollector").finish()
    }
}

impl Collector for PrometheusStateCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.gauge.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let counts = self.state.count_by_phase();
        for phase in ALL_PHASES {
            let count = counts.get(phase).copied().unwrap_or(0);
            self.gauge
                .with_label_values(&[phase_label(*phase)])
                .set(count as f64);
        }
        self.gauge.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;
    use solti_model::{TaskId, TaskKind, TaskSpec};
    use std::sync::Arc;

    fn spec() -> TaskSpec {
        TaskSpec::builder("slot", TaskKind::Embedded, 5_000_u64)
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
        let collector = PrometheusStateCollector::new(state).unwrap();

        let families = collector.collect();
        for phase in [
            "pending",
            "running",
            "succeeded",
            "failed",
            "timeout",
            "canceled",
            "exhausted",
        ] {
            assert_eq!(
                gauge_value(&families, "solti_sv_tasks_by_phase", phase),
                Some(0.0),
                "phase {phase} must be zero on empty state",
            );
        }
    }

    #[test]
    fn collector_counts_pending_tasks() {
        let state = TaskState::new();
        state.seed_task(TaskId::from("t1"), spec());
        state.seed_task(TaskId::from("t2"), spec());
        state.seed_task(TaskId::from("t3"), spec());

        let collector = PrometheusStateCollector::new(state).unwrap();
        let families = collector.collect();

        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "pending"),
            Some(3.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "running"),
            Some(0.0)
        );
    }

    #[test]
    fn collector_reflects_transitions() {
        let state = TaskState::new();
        state.seed_task(TaskId::from("t1"), spec());
        state.seed_task(TaskId::from("t2"), spec());
        state.seed_starting(&TaskId::from("t1"));

        let collector = PrometheusStateCollector::new(state.clone()).unwrap();
        let families = collector.collect();

        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "pending"),
            Some(1.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "running"),
            Some(1.0)
        );

        state.seed_finished(&TaskId::from("t1"), TaskPhase::Succeeded, None, None);
        let families = collector.collect();
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "running"),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "succeeded"),
            Some(1.0)
        );
    }

    #[test]
    fn collector_registers_into_registry_and_scrapes() {
        let registry = Arc::new(Registry::new());
        let state = TaskState::new();
        state.seed_task(TaskId::from("alpha"), spec());
        state.seed_starting(&TaskId::from("alpha"));

        let collector = PrometheusStateCollector::new(state).unwrap();
        registry.register(Box::new(collector)).unwrap();

        let families = registry.gather();
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "running"),
            Some(1.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_sv_tasks_by_phase", "pending"),
            Some(0.0)
        );
    }
}
