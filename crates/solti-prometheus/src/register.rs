//! Private helpers for building and registering Prometheus metrics.

#[cfg(any(feature = "api", feature = "discover", feature = "taskvisor"))]
use prometheus::HistogramOpts;
use prometheus::Opts;
#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
use prometheus::{Registry, core::Collector, core::Desc, proto::MetricFamily};

#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
use prometheus::CounterVec;
#[cfg(feature = "api")]
use prometheus::GaugeVec;
#[cfg(any(feature = "api", feature = "discover"))]
use prometheus::HistogramVec;
#[cfg(any(feature = "discover", feature = "taskvisor"))]
use prometheus::{Counter, Gauge, Histogram};

/// One atomically registered group of related collectors.
#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
pub(crate) struct MetricGroup {
    collectors: Vec<Box<dyn Collector>>,
}

#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
impl MetricGroup {
    pub(crate) fn new() -> Self {
        Self {
            collectors: Vec::new(),
        }
    }

    fn opts(subsystem: &'static str, name: &str, help: &str) -> Opts {
        Opts::new(name, help)
            .namespace("solti")
            .subsystem(subsystem)
    }

    #[cfg(any(feature = "api", feature = "discover", feature = "taskvisor"))]
    fn hist_opts(
        subsystem: &'static str,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
    ) -> HistogramOpts {
        HistogramOpts::new(name, help)
            .namespace("solti")
            .subsystem(subsystem)
            .buckets(buckets)
    }

    fn retain<C>(&mut self, collector: &C)
    where
        C: Collector + Clone + 'static,
    {
        self.collectors.push(Box::new(collector.clone()));
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn counter(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
    ) -> Result<Counter, prometheus::Error> {
        let counter = Counter::with_opts(Self::opts(subsystem, name, help))?;
        self.retain(&counter);
        Ok(counter)
    }

    pub(crate) fn counter_vec(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<CounterVec, prometheus::Error> {
        let counter = CounterVec::new(Self::opts(subsystem, name, help), labels)?;
        self.retain(&counter);
        Ok(counter)
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn gauge(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
    ) -> Result<Gauge, prometheus::Error> {
        let gauge = Gauge::with_opts(Self::opts(subsystem, name, help))?;
        self.retain(&gauge);
        Ok(gauge)
    }

    #[cfg(feature = "api")]
    pub(crate) fn gauge_vec(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<GaugeVec, prometheus::Error> {
        let gauge = GaugeVec::new(Self::opts(subsystem, name, help), labels)?;
        self.retain(&gauge);
        Ok(gauge)
    }

    #[cfg(any(feature = "discover", feature = "taskvisor"))]
    pub(crate) fn histogram(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
    ) -> Result<Histogram, prometheus::Error> {
        let histogram = Histogram::with_opts(Self::hist_opts(subsystem, name, help, buckets))?;
        self.retain(&histogram);
        Ok(histogram)
    }

    #[cfg(any(feature = "api", feature = "discover"))]
    pub(crate) fn histogram_vec(
        &mut self,
        subsystem: &'static str,
        name: &str,
        help: &str,
        buckets: Vec<f64>,
        labels: &[&str],
    ) -> Result<HistogramVec, prometheus::Error> {
        let histogram = HistogramVec::new(Self::hist_opts(subsystem, name, help, buckets), labels)?;
        self.retain(&histogram);
        Ok(histogram)
    }

    pub(crate) fn register(self, registry: &Registry) -> Result<(), prometheus::Error> {
        registry.register(Box::new(self))
    }
}

#[cfg(any(
    feature = "api",
    feature = "discover",
    feature = "runner",
    feature = "taskvisor"
))]
impl Collector for MetricGroup {
    fn desc(&self) -> Vec<&Desc> {
        self.collectors
            .iter()
            .flat_map(|collector| collector.desc())
            .collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.collectors
            .iter()
            .flat_map(|collector| collector.collect())
            .collect()
    }
}

/// Build a [`GaugeVec`] without registering it into a registry.
#[cfg(feature = "state")]
pub(crate) fn gauge_vec_unregistered(
    subsystem: &'static str,
    name: &str,
    help: &str,
    labels: &[&str],
) -> Result<prometheus::GaugeVec, prometheus::Error> {
    let opts = Opts::new(name, help)
        .namespace("solti")
        .subsystem(subsystem);
    prometheus::GaugeVec::new(opts, labels)
}

/// Convert milliseconds to seconds for histogram observation.
#[cfg(any(feature = "api", feature = "discover", feature = "taskvisor"))]
#[inline]
pub(crate) fn ms_to_secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}
