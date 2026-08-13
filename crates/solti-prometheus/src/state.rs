//! # Core state metrics
//!
//! [`PrometheusCoreStateCollector`] counts stored tasks by [`TaskPhase`].
//! It reads [`TaskState`] when Prometheus collects the metric.
//!
//! Enable it with the `state` feature.
//!
//! ## Flow
//!
//! ```text
//! TaskState ──► count_by_phase() ──► solti_core_tasks_by_phase ──► Registry
//! ```

use std::sync::Mutex;

use prometheus::GaugeVec;
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use solti_core::TaskState;
use solti_model::TaskPhase;

use crate::register::gauge_vec_unregistered;

/// Phases emitted as separate gauge series.
///
/// Other variants use `phase="unknown"`.
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

/// Pull-based metrics for the current `TaskState`.
///
/// ## Metric
///
/// | Metric                      | Type  | Labels  | Description             |
/// |-----------------------------|-------|---------|-------------------------|
/// | `solti_core_tasks_by_phase` | Gauge | `phase` | Stored tasks by phase   |
///
/// ## Collection
///
/// Every scrape recounts all tasks currently stored in [`TaskState`].
/// Known phases are emitted even when their count is zero.
/// Other phase variants are aggregated under `phase="unknown"`.
///
/// Terminal tasks remain visible while `TaskState` retains them.
///
/// ```text
/// Pending   ─┐
/// Running   ─┤
/// Succeeded ─┤
/// Failed    ─┼──► tasks_by_phase{phase}
/// Timeout   ─┤
/// Canceled  ─┤
/// Exhausted ─┤
/// other     ─┘    phase="unknown"
/// ```
///
/// ## Example
///
/// ```rust
/// use solti_core::TaskState;
/// use solti_prometheus::{PrometheusCoreStateCollector, Registry};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
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
    /// Creates a collector for one shared task state.
    ///
    /// The collector is not registered by this method.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the gauge descriptor cannot be created.
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
    fn collector_reflects_transitions() {
        let state = TaskState::new();
        let task_id = TaskId::new("t1").unwrap();
        state.seed_task(task_id.clone(), spec());
        state.seed_task(TaskId::new("t2").unwrap(), spec());
        state.seed_starting(&task_id);

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

        state.seed_finished(&task_id, TaskPhase::Succeeded, None, None);
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
        let registry = Registry::new();
        let state = TaskState::new();
        let task_id = TaskId::new("alpha").unwrap();
        state.seed_task(task_id.clone(), spec());
        state.seed_starting(&task_id);

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
