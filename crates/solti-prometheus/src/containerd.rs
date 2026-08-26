//! # Containerd local runtime metrics
//!
//! [`PrometheusContainerdCollector`] exports synchronous local cleanup and I/O
//! worker status. It does not probe the containerd daemon or report individual
//! container attempts.
//!
//! `healthy=0` is the complete local worker failure signal. `quarantined=1`
//! identifies the narrower case where a domain explicitly recorded terminal
//! quarantine; other fail-closed failures can retain `owned>0` without it.

use std::sync::{Arc, Mutex};

use prometheus::{GaugeVec, Opts};
use prometheus::{core::Collector, core::Desc, proto::MetricFamily};
use solti_exec::container::containerd::ContainerdEngine;

#[derive(Clone, Copy)]
struct WorkerSnapshot {
    accepting: bool,
    healthy: bool,
    owned: usize,
    capacity: usize,
    quarantined: bool,
}

#[derive(Clone, Copy)]
struct RuntimeSnapshot {
    cleanup: WorkerSnapshot,
    io: WorkerSnapshot,
}

trait ContainerdStatusSource: Send + Sync + 'static {
    fn runtime_snapshot(&self) -> RuntimeSnapshot;
}

impl ContainerdStatusSource for ContainerdEngine {
    fn runtime_snapshot(&self) -> RuntimeSnapshot {
        let status = self.runtime_status();
        let cleanup = status.cleanup();
        let io = status.io();
        RuntimeSnapshot {
            cleanup: WorkerSnapshot {
                accepting: cleanup.accepting(),
                healthy: cleanup.healthy(),
                owned: cleanup.owned(),
                capacity: cleanup.capacity(),
                quarantined: cleanup.quarantined(),
            },
            io: WorkerSnapshot {
                accepting: io.accepting(),
                healthy: io.healthy(),
                owned: io.owned(),
                capacity: io.capacity(),
                quarantined: io.quarantined(),
            },
        }
    }
}

/// Pull-based metrics for one native containerd engine's local worker domains.
///
/// The application-provided engine name is emitted as the constant `engine`
/// label. `domain` is bounded to `cleanup` and `io`. These metrics describe
/// process-local lifecycle workers; they do not prove containerd daemon,
/// snapshotter, or runtime availability.
///
/// ## Metrics
///
/// | Metric                                      | Type  | Labels             |
/// |---------------------------------------------|-------|--------------------|
/// | `solti_exec_containerd_worker_accepting`    | Gauge | `engine`, `domain` |
/// | `solti_exec_containerd_worker_healthy`      | Gauge | `engine`, `domain` |
/// | `solti_exec_containerd_worker_owned`        | Gauge | `engine`, `domain` |
/// | `solti_exec_containerd_worker_capacity`     | Gauge | `engine`, `domain` |
/// | `solti_exec_containerd_worker_quarantined`  | Gauge | `engine`, `domain` |
pub struct PrometheusContainerdCollector {
    source: Arc<dyn ContainerdStatusSource>,
    accepting: GaugeVec,
    healthy: GaugeVec,
    owned: GaugeVec,
    capacity: GaugeVec,
    quarantined: GaugeVec,
    collect_lock: Mutex<()>,
}

impl PrometheusContainerdCollector {
    /// Creates an unregistered collector for one native containerd engine.
    ///
    /// Register the returned collector with the same shared registry used by
    /// the binary's other Solti metrics. `engine_name` must be a stable,
    /// application-controlled label.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when a metric descriptor cannot be created.
    pub fn new(
        engine_name: impl Into<String>,
        engine: Arc<ContainerdEngine>,
    ) -> Result<Self, prometheus::Error> {
        Self::from_source(engine_name.into(), engine)
    }

    fn from_source(
        engine_name: String,
        source: Arc<dyn ContainerdStatusSource>,
    ) -> Result<Self, prometheus::Error> {
        let accepting = gauge_vec(
            "accepting",
            "Whether local containerd worker admission is open",
            &engine_name,
        )?;
        let healthy = gauge_vec(
            "healthy",
            "Whether the local containerd worker has preserved ownership forward progress",
            &engine_name,
        )?;
        let owned = gauge_vec(
            "owned",
            "Current ownership charged to the local containerd worker domain",
            &engine_name,
        )?;
        let capacity = gauge_vec(
            "capacity",
            "Configured ownership limit of the local containerd worker domain",
            &engine_name,
        )?;
        let quarantined = gauge_vec(
            "quarantined",
            "Whether the local containerd worker domain explicitly recorded terminal quarantine",
            &engine_name,
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

    fn set_domain(&self, domain: &str, status: WorkerSnapshot) {
        self.accepting
            .with_label_values(&[domain])
            .set(bool_gauge(status.accepting));
        self.healthy
            .with_label_values(&[domain])
            .set(bool_gauge(status.healthy));
        self.owned
            .with_label_values(&[domain])
            .set(status.owned as f64);
        self.capacity
            .with_label_values(&[domain])
            .set(status.capacity as f64);
        self.quarantined
            .with_label_values(&[domain])
            .set(bool_gauge(status.quarantined));
    }
}

impl std::fmt::Debug for PrometheusContainerdCollector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrometheusContainerdCollector")
            .finish()
    }
}

impl Collector for PrometheusContainerdCollector {
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
        let status = self.source.runtime_snapshot();
        self.set_domain("cleanup", status.cleanup);
        self.set_domain("io", status.io);
        self.collectors()
            .into_iter()
            .flat_map(|collector| collector.collect())
            .collect()
    }
}

fn gauge_vec(name: &str, help: &str, engine_name: &str) -> Result<GaugeVec, prometheus::Error> {
    GaugeVec::new(
        Opts::new(name, help)
            .namespace("solti")
            .subsystem("exec_containerd_worker")
            .const_label("engine", engine_name.to_owned()),
        &["domain"],
    )
}

const fn bool_gauge(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Registry;

    struct FixedSource(Mutex<RuntimeSnapshot>);

    impl ContainerdStatusSource for FixedSource {
        fn runtime_snapshot(&self) -> RuntimeSnapshot {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    fn gauge_value(
        families: &[MetricFamily],
        name: &str,
        engine: &str,
        domain: &str,
    ) -> Option<f64> {
        families
            .iter()
            .find(|family| family.name() == name)?
            .get_metric()
            .iter()
            .find(|metric| {
                let labels = metric.get_label();
                labels
                    .iter()
                    .any(|label| label.name() == "engine" && label.value() == engine)
                    && labels
                        .iter()
                        .any(|label| label.name() == "domain" && label.value() == domain)
            })
            .map(|metric| metric.get_gauge().value())
    }

    fn source(status: RuntimeSnapshot) -> Arc<FixedSource> {
        Arc::new(FixedSource(Mutex::new(status)))
    }

    fn healthy(capacity: usize) -> WorkerSnapshot {
        WorkerSnapshot {
            accepting: true,
            healthy: true,
            owned: 0,
            capacity,
            quarantined: false,
        }
    }

    #[test]
    fn collector_exports_cleanup_and_io_status() {
        let registry = Registry::new();
        let source = source(RuntimeSnapshot {
            cleanup: WorkerSnapshot {
                accepting: false,
                healthy: false,
                owned: 5,
                capacity: 8,
                quarantined: true,
            },
            io: healthy(8),
        });
        let collector = PrometheusContainerdCollector::from_source(
            "main".to_owned(),
            Arc::clone(&source) as Arc<dyn ContainerdStatusSource>,
        )
        .unwrap();
        registry.register(Box::new(collector)).unwrap();

        let families = registry.gather();
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_containerd_worker_accepting",
                "main",
                "cleanup"
            ),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_containerd_worker_healthy",
                "main",
                "cleanup"
            ),
            Some(0.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_containerd_worker_owned",
                "main",
                "cleanup"
            ),
            Some(5.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_containerd_worker_capacity",
                "main",
                "io"
            ),
            Some(8.0)
        );
        assert_eq!(
            gauge_value(
                &families,
                "solti_exec_containerd_worker_quarantined",
                "main",
                "cleanup"
            ),
            Some(1.0)
        );

        *source
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RuntimeSnapshot {
            cleanup: healthy(8),
            io: WorkerSnapshot {
                accepting: true,
                healthy: true,
                owned: 3,
                capacity: 8,
                quarantined: false,
            },
        };
        let refreshed = registry.gather();
        assert_eq!(
            gauge_value(
                &refreshed,
                "solti_exec_containerd_worker_owned",
                "main",
                "io"
            ),
            Some(3.0)
        );
        assert_eq!(
            gauge_value(
                &refreshed,
                "solti_exec_containerd_worker_healthy",
                "main",
                "cleanup"
            ),
            Some(1.0)
        );
    }

    #[test]
    fn collectors_for_distinct_engines_share_one_registry() {
        let registry = Registry::new();
        for engine in ["first", "second"] {
            let collector = PrometheusContainerdCollector::from_source(
                engine.to_owned(),
                source(RuntimeSnapshot {
                    cleanup: healthy(4),
                    io: healthy(4),
                }),
            )
            .unwrap();
            registry.register(Box::new(collector)).unwrap();
        }

        let families = registry.gather();
        assert_eq!(
            families
                .iter()
                .find(|family| family.name() == "solti_exec_containerd_worker_capacity")
                .unwrap()
                .get_metric()
                .len(),
            4
        );
    }
}
