//! # Subprocess finalizer metrics
//!
//! [`PrometheusSubprocessFinalizerCollector`] exports the synchronous status of
//! one subprocess runner's bounded drop finalizer. It does not report the
//! status of individual task attempts or the runner's separate cwd worker.

use std::sync::{Arc, Mutex};

use prometheus::{Gauge, Opts};
use prometheus::{core::Collector, core::Desc, proto::MetricFamily};
use solti_exec::subprocess::SubprocessRunner;

#[derive(Clone, Copy)]
struct FinalizerSnapshot {
    accepting: bool,
    healthy: bool,
    owned: usize,
    capacity: usize,
    quarantined: usize,
}

trait FinalizerStatusSource: Send + Sync + 'static {
    fn finalizer_snapshot(&self) -> FinalizerSnapshot;
}

impl FinalizerStatusSource for SubprocessRunner {
    fn finalizer_snapshot(&self) -> FinalizerSnapshot {
        let status = self.finalizer_status();
        FinalizerSnapshot {
            accepting: status.accepting(),
            healthy: status.healthy(),
            owned: status.owned(),
            capacity: status.capacity(),
            quarantined: status.quarantined(),
        }
    }
}

/// Pull-based metrics for one subprocess runner's bounded drop finalizer.
///
/// The runner name is emitted as the constant `runner` label. Separate
/// collectors with distinct runner names can be registered in one registry.
/// `owned` includes active, queued, finalizing, and quarantined cleanup
/// ownership; it is not the number of currently running processes.
///
/// ## Metrics
///
/// | Metric                                            | Type  | Labels   |
/// |---------------------------------------------------|-------|----------|
/// | `solti_exec_subprocess_finalizer_accepting`       | Gauge | `runner` |
/// | `solti_exec_subprocess_finalizer_healthy`         | Gauge | `runner` |
/// | `solti_exec_subprocess_finalizer_owned`           | Gauge | `runner` |
/// | `solti_exec_subprocess_finalizer_capacity`        | Gauge | `runner` |
/// | `solti_exec_subprocess_finalizer_quarantined`     | Gauge | `runner` |
pub struct PrometheusSubprocessFinalizerCollector {
    source: Arc<dyn FinalizerStatusSource>,
    accepting: Gauge,
    healthy: Gauge,
    owned: Gauge,
    capacity: Gauge,
    quarantined: Gauge,
    collect_lock: Mutex<()>,
}

impl PrometheusSubprocessFinalizerCollector {
    /// Creates an unregistered collector for `runner`.
    ///
    /// Register the returned collector with the same shared registry used by
    /// the binary's other Solti metrics. `runner_name` must be a stable,
    /// application-controlled label such as the name used during runner
    /// registration.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when a metric descriptor cannot be created.
    pub fn new(
        runner_name: impl Into<String>,
        runner: Arc<SubprocessRunner>,
    ) -> Result<Self, prometheus::Error> {
        Self::from_source(runner_name.into(), runner)
    }

    fn from_source(
        runner_name: String,
        source: Arc<dyn FinalizerStatusSource>,
    ) -> Result<Self, prometheus::Error> {
        let accepting = gauge(
            "accepting",
            "Whether subprocess finalizer admission is open",
            &runner_name,
        )?;
        let healthy = gauge(
            "healthy",
            "Whether the subprocess finalizer has preserved cleanup forward progress",
            &runner_name,
        )?;
        let owned = gauge(
            "owned",
            "Current subprocess cleanup ownership charged to the finalizer",
            &runner_name,
        )?;
        let capacity = gauge(
            "capacity",
            "Configured subprocess finalizer ownership limit",
            &runner_name,
        )?;
        let quarantined = gauge(
            "quarantined",
            "Current subprocess cleanup ownership retained in terminal quarantine",
            &runner_name,
        )?;
        Ok(Self {
            source,
            accepting,
            healthy,
            owned,
            capacity,
            quarantined,
            collect_lock: Mutex::new(()),
        })
    }

    fn collectors(&self) -> [&dyn Collector; 5] {
        [
            &self.accepting,
            &self.healthy,
            &self.owned,
            &self.capacity,
            &self.quarantined,
        ]
    }
}

impl std::fmt::Debug for PrometheusSubprocessFinalizerCollector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrometheusSubprocessFinalizerCollector")
            .finish()
    }
}

impl Collector for PrometheusSubprocessFinalizerCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.collectors()
            .into_iter()
            .flat_map(|collector| collector.desc())
            .collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let _collect = self
            .collect_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = self.source.finalizer_snapshot();
        self.accepting.set(bool_gauge(status.accepting));
        self.healthy.set(bool_gauge(status.healthy));
        self.owned.set(status.owned as f64);
        self.capacity.set(status.capacity as f64);
        self.quarantined.set(status.quarantined as f64);
        self.collectors()
            .into_iter()
            .flat_map(|collector| collector.collect())
            .collect()
    }
}

fn gauge(name: &str, help: &str, runner_name: &str) -> Result<Gauge, prometheus::Error> {
    Gauge::with_opts(
        Opts::new(name, help)
            .namespace("solti")
            .subsystem("exec_subprocess_finalizer")
            .const_label("runner", runner_name.to_owned()),
    )
}

const fn bool_gauge(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;

    struct FixedSource(Mutex<FinalizerSnapshot>);

    impl FinalizerStatusSource for FixedSource {
        fn finalizer_snapshot(&self) -> FinalizerSnapshot {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    fn gauge_value(families: &[MetricFamily], name: &str, runner: &str) -> Option<f64> {
        families
            .iter()
            .find(|family| family.name() == name)?
            .get_metric()
            .iter()
            .find(|metric| {
                metric
                    .get_label()
                    .iter()
                    .any(|label| label.name() == "runner" && label.value() == runner)
            })
            .map(|metric| metric.get_gauge().value())
    }

    fn source(status: FinalizerSnapshot) -> Arc<FixedSource> {
        Arc::new(FixedSource(Mutex::new(status)))
    }

    #[test]
    fn collector_exports_every_finalizer_status_value() {
        let registry = Registry::new();
        let status = FinalizerSnapshot {
            accepting: false,
            healthy: false,
            owned: 7,
            capacity: 16,
            quarantined: 2,
        };
        let source = source(status);
        let collector = PrometheusSubprocessFinalizerCollector::from_source(
            "local".to_owned(),
            Arc::clone(&source) as Arc<dyn FinalizerStatusSource>,
        )
        .unwrap();
        registry.register(Box::new(collector)).unwrap();

        let families = registry.gather();
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_subprocess_finalizer_accepting",
                "local"
            ),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_subprocess_finalizer_healthy",
                "local"
            ),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(&families, "solti_exec_subprocess_finalizer_owned", "local"),
            Some(7.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_subprocess_finalizer_capacity",
                "local"
            ),
            Some(16.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_subprocess_finalizer_quarantined",
                "local"
            ),
            Some(2.0)
        );

        *source
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = FinalizerSnapshot {
            accepting: true,
            healthy: true,
            owned: 1,
            capacity: 16,
            quarantined: 0,
        };
        let refreshed = registry.gather();
        assert_eq!(
            gauge_value(
                &refreshed,
                "solti_exec_subprocess_finalizer_accepting",
                "local"
            ),
            Some(1.0)
        );
        assert_eq!(
            gauge_value(&refreshed, "solti_exec_subprocess_finalizer_owned", "local"),
            Some(1.0)
        );
    }

    #[test]
    fn collectors_for_distinct_runners_share_one_registry() {
        let registry = Registry::new();
        for runner in ["first", "second"] {
            let collector = PrometheusSubprocessFinalizerCollector::from_source(
                runner.to_owned(),
                source(FinalizerSnapshot {
                    accepting: true,
                    healthy: true,
                    owned: 0,
                    capacity: 4,
                    quarantined: 0,
                }),
            )
            .unwrap();
            registry.register(Box::new(collector)).unwrap();
        }

        let families = registry.gather();
        assert_eq!(
            families
                .iter()
                .find(|family| family.name() == "solti_exec_subprocess_finalizer_capacity")
                .unwrap()
                .get_metric()
                .len(),
            2
        );
    }
}
