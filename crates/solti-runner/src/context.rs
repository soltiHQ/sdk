//! # Build context
//!
//! [`BuildContext`] carries dependencies shared by registered runners.
//!
//! ## Flow
//!
//! ```text
//! RunnerRouter
//!      └── BuildContext ──▶ Runner::build_task
//!               ├── environment
//!               ├── metrics
//!               └── output publisher
//! ```

use std::fmt;

use crate::RunnerEnv;
use crate::metrics::{MetricsHandle, panic_contained_metrics};
use crate::output::{
    OutputPublisherHandle, noop_output_publisher, panic_contained_output_publisher,
};

/// Dependencies passed to [`Runner::build_task`](crate::Runner::build_task).
///
/// Create the context when the router is assembled.
/// [`RunnerRouter::with_context`](crate::RunnerRouter::with_context) installs it.
/// Metrics and output publisher handles are installed behind separate sticky
/// panic boundaries. A boundary stops invoking its callback after the first
/// observed panic and is shared by every clone of this context. Calls that
/// already entered the boundary concurrently may still finish or panic; the
/// boundary does not serialize healthy callbacks.
///
/// ## Defaults
///
/// | Dependency         | Default                              |
/// |--------------------|--------------------------------------|
/// | Environment        | Empty [`RunnerEnv`]                  |
/// | Metrics            | [`NoOpMetrics`](crate::NoOpMetrics)  |
/// | Output publisher   | No-op publisher                      |
///
/// ## Example
///
/// ```
/// use solti_runner::{BuildContext, RunnerEnv, noop_metrics};
///
/// let mut env = RunnerEnv::new();
/// env.push("PATH", "/usr/bin");
///
/// let ctx = BuildContext::new(env, noop_metrics());
/// assert_eq!(ctx.env().get("PATH"), Some("/usr/bin"));
/// ```
///
/// ## See Also
///
/// - [`RunnerRouter::with_context`](crate::RunnerRouter::with_context)
/// - [`OutputPublisherHandle`]
/// - [`MetricsHandle`]
#[derive(Clone)]
pub struct BuildContext {
    output_publisher: OutputPublisherHandle,
    metrics: MetricsHandle,
    env: RunnerEnv,
}

impl BuildContext {
    /// Creates a context with the given environment and metrics.
    ///
    /// Output publishing is disabled until replaced.
    pub fn new(env: RunnerEnv, metrics: MetricsHandle) -> Self {
        Self {
            env,
            metrics: panic_contained_metrics(metrics),
            output_publisher: panic_contained_output_publisher(noop_output_publisher()),
        }
    }

    /// Returns the shared runner environment.
    pub fn env(&self) -> &RunnerEnv {
        &self.env
    }

    /// Returns the shared metrics handle.
    pub fn metrics(&self) -> &MetricsHandle {
        &self.metrics
    }

    /// Returns the shared output publisher.
    pub fn output_publisher(&self) -> &OutputPublisherHandle {
        &self.output_publisher
    }

    /// Replaces the environment.
    pub fn with_env(mut self, env: RunnerEnv) -> Self {
        self.env = env;
        self
    }

    /// Replaces the metrics backend and installs a new sticky panic boundary.
    pub fn with_metrics(mut self, metrics: MetricsHandle) -> Self {
        self.metrics = panic_contained_metrics(metrics);
        self
    }

    /// Replaces the output publisher and installs a new sticky panic boundary.
    pub fn with_output_publisher(mut self, publisher: OutputPublisherHandle) -> Self {
        self.output_publisher = panic_contained_output_publisher(publisher);
        self
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self {
            env: RunnerEnv::default(),
            metrics: panic_contained_metrics(crate::metrics::noop_metrics()),
            output_publisher: panic_contained_output_publisher(noop_output_publisher()),
        }
    }
}

impl fmt::Debug for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildContext")
            .field("env_len", &self.env.len())
            .field("metrics", &"<handle>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::BuildContext;
    use crate::{
        MetricsBackend, MetricsHandle, OutputPublisher, OutputPublisherHandle, OutputSink,
        RunnerEnv, RunnerErrorKind, RunnerType,
    };

    use solti_model::TaskId;

    #[derive(Default)]
    struct TestMetrics {
        calls: AtomicUsize,
    }

    impl MetricsBackend for TestMetrics {
        fn record_runner_error(&self, _runner_type: RunnerType, _error_kind: RunnerErrorKind) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct TestPublisher;

    impl OutputPublisher for TestPublisher {
        fn sink_for(
            &self,
            _task_name: &TaskId,
            generation: u64,
            attempt: u32,
        ) -> Option<OutputSink> {
            Some(OutputSink::new(generation, attempt, |_| {}))
        }
    }

    #[test]
    fn default_context_has_empty_env_and_disables_output() {
        let ctx = BuildContext::default();
        assert!(ctx.env().is_empty());
        assert!(
            ctx.output_publisher()
                .sink_for(&TaskId::new("task").unwrap(), 1, 1)
                .is_none()
        );
    }

    #[test]
    fn constructor_and_builders_replace_env_and_metrics() {
        let mut initial_env = RunnerEnv::new();
        initial_env.push("FOO", "one");
        let initial_metrics = Arc::new(TestMetrics::default());
        let initial_handle: MetricsHandle = initial_metrics.clone();
        let ctx = BuildContext::new(initial_env, initial_handle);

        ctx.metrics()
            .record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        assert_eq!(initial_metrics.calls.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.env().get("FOO"), Some("one"));

        let mut replacement_env = RunnerEnv::new();
        replacement_env.push("BAR", "two");
        let replacement_metrics = Arc::new(TestMetrics::default());
        let replacement_handle: MetricsHandle = replacement_metrics.clone();
        let ctx = ctx
            .with_env(replacement_env)
            .with_metrics(replacement_handle);

        ctx.metrics()
            .record_runner_error(RunnerType::Wasm, RunnerErrorKind::ModuleLoadFailed);
        assert_eq!(initial_metrics.calls.load(Ordering::Relaxed), 1);
        assert_eq!(replacement_metrics.calls.load(Ordering::Relaxed), 1);
        assert!(ctx.env().get("FOO").is_none());
        assert_eq!(ctx.env().get("BAR"), Some("two"));
    }

    #[test]
    fn output_publisher_builder_exposes_attempt_sink() {
        let publisher: OutputPublisherHandle = Arc::new(TestPublisher);
        let ctx = BuildContext::default().with_output_publisher(Arc::clone(&publisher));
        let sink = ctx
            .output_publisher()
            .sink_for(&TaskId::new("task").unwrap(), 3, 7)
            .expect("enabled sink");

        assert_eq!(sink.attempt(), 7);
        assert_eq!(sink.generation(), 3);
    }
}
