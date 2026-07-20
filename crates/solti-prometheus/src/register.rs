//! Private helpers for building and registering Prometheus metrics.
//!
//! All solti collectors share the `solti_<subsystem>_<name>` naming scheme and the same "build -> register clone" boilerplate.
//! [`Sub`] binds a [`Registry`] and a subsystem name together so constructors collapse to one line per metric.

#[cfg(any(feature = "api", feature = "state"))]
use prometheus::GaugeVec;
#[cfg(any(feature = "api", feature = "discover", feature = "runner"))]
use prometheus::HistogramVec;
use prometheus::Opts;
#[cfg(any(feature = "discover", feature = "taskvisor"))]
use prometheus::{Counter, Gauge, Histogram};
#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
use prometheus::{CounterVec, HistogramOpts, Registry};

/// Scoped registrar bound to a `solti` subsystem (e.g. `runner`, `sv`, `api`).
#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
pub(crate) struct Sub<'a> {
    registry: &'a Registry,
    subsystem: &'static str,
}

#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
impl<'a> Sub<'a> {
    pub(crate) fn new(registry: &'a Registry, subsystem: &'static str) -> Self {
        Self {
            registry,
            subsystem,
        }
    }

    fn opts(&self, name: &str, help: &str) -> Opts {
        Opts::new(name, help)
            .namespace("solti")
            .subsystem(self.subsystem)
    }

    fn hist_opts(&self, name: &str, help: &str, buckets: Vec<f64>) -> HistogramOpts {
        HistogramOpts::new(name, help)
            .namespace("solti")
            .subsystem(self.subsystem)
            .buckets(buckets)
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn counter(&self, name: &str, help: &str) -> Result<Counter, prometheus::Error> {
        let c = Counter::with_opts(self.opts(name, help))?;
        self.registry.register(Box::new(c.clone()))?;
        Ok(c)
    }

    pub(crate) fn counter_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<CounterVec, prometheus::Error> {
        let c = CounterVec::new(self.opts(name, help), labels)?;
        self.registry.register(Box::new(c.clone()))?;
        Ok(c)
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn gauge(&self, name: &str, help: &str) -> Result<Gauge, prometheus::Error> {
        let g = Gauge::with_opts(self.opts(name, help))?;
        self.registry.register(Box::new(g.clone()))?;
        Ok(g)
    }

    #[cfg(feature = "api")]
    pub(crate) fn gauge_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<GaugeVec, prometheus::Error> {
        let g = GaugeVec::new(self.opts(name, help), labels)?;
        self.registry.register(Box::new(g.clone()))?;
        Ok(g)
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn histogram(
        &self,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
    ) -> Result<Histogram, prometheus::Error> {
        let h = Histogram::with_opts(self.hist_opts(name, help, buckets))?;
        self.registry.register(Box::new(h.clone()))?;
        Ok(h)
    }

    #[cfg(any(feature = "api", feature = "discover", feature = "runner"))]
    pub(crate) fn histogram_vec(
        &self,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
        labels: &[&str],
    ) -> Result<HistogramVec, prometheus::Error> {
        let h = HistogramVec::new(self.hist_opts(name, help, buckets), labels)?;
        self.registry.register(Box::new(h.clone()))?;
        Ok(h)
    }
}

/// Build a [`GaugeVec`] without registering it into a registry.
///
/// Composite collectors register themselves; registering this inner metric as
/// well would register it twice.
#[cfg(feature = "state")]
pub(crate) fn gauge_vec_unregistered(
    subsystem: &'static str,
    name: &str,
    help: &str,
    labels: &[&str],
) -> Result<GaugeVec, prometheus::Error> {
    let opts = Opts::new(name, help)
        .namespace("solti")
        .subsystem(subsystem);
    GaugeVec::new(opts, labels)
}

/// Convert milliseconds to seconds for histogram observation.
#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
#[inline]
pub(crate) fn ms_to_secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}
