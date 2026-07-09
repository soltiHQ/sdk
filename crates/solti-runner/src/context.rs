//! # Build context.
//!
//! [`BuildContext`] carries shared dependencies injected into runners at
//! task-build time.
//!
//! See [`Runner::build_task`](crate::Runner::build_task) for usage.

use std::fmt;
use std::sync::Arc;

use solti_model::RunnerEnv;

use crate::metrics::MetricsHandle;
use crate::output::OutputRegistry;

/// Shared build context passed to all runners.
///
/// It carries:
/// - a metrics handle,
/// - runner environment variables,
/// - an output registry for live stdout/stderr streaming.
///
/// Create it once at router setup time and pass it to [`RunnerRouter::with_context`](crate::RunnerRouter::with_context).
///
/// ## Defaults
///
/// - `env`: empty [`RunnerEnv`]
/// - `metrics`: [`NoOpMetrics`](crate::NoOpMetrics) (zero-cost)
/// - `output_registry`: a fresh, empty [`OutputRegistry`](crate::OutputRegistry) (no live subscribers)
///
/// ## Also
///
/// - [`RunnerRouter::with_context`](crate::RunnerRouter::with_context) sets the context for all runners.
/// - [`MetricsHandle`](crate::MetricsHandle) - `Arc<dyn MetricsBackend>`.
///
/// ## Example
///
/// ```
/// use solti_model::RunnerEnv;
/// use solti_runner::{BuildContext, noop_metrics};
///
/// let mut env = RunnerEnv::new();
/// env.push("PATH", "/usr/bin");
///
/// let ctx = BuildContext::new(env, noop_metrics());
/// assert_eq!(ctx.env().get("PATH"), Some("/usr/bin"));
/// ```
#[derive(Clone)]
pub struct BuildContext {
    output_registry: Arc<OutputRegistry>,
    metrics: MetricsHandle,
    env: RunnerEnv,
}

impl BuildContext {
    /// Create a new build context with the given env and metrics.
    ///
    /// Starts with a fresh, empty [`OutputRegistry`].
    /// To share the supervisor's live output registry, chain [`with_output_registry`](Self::with_output_registry).
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::RunnerEnv;
    /// use solti_runner::{BuildContext, noop_metrics};
    ///
    /// let ctx = BuildContext::new(RunnerEnv::new(), noop_metrics());
    /// assert_eq!(ctx.env().len(), 0);
    /// ```
    pub fn new(env: RunnerEnv, metrics: MetricsHandle) -> Self {
        Self {
            env,
            metrics,
            output_registry: Arc::new(OutputRegistry::default()),
        }
    }

    /// Get a reference to the shared environment.
    ///
    /// ## Example
    ///
    /// ```
    /// let ctx = solti_runner::BuildContext::default();
    /// assert_eq!(ctx.env().len(), 0);
    /// ```
    pub fn env(&self) -> &RunnerEnv {
        &self.env
    }

    /// Get a clonable handle to the metrics backend.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::{BuildContext, RunnerType};
    ///
    /// let ctx = BuildContext::default();
    /// ctx.metrics().record_task_started(RunnerType::Subprocess);
    /// ```
    pub fn metrics(&self) -> &MetricsHandle {
        &self.metrics
    }

    /// Get a shared handle to the output registry.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskId;
    /// use solti_runner::BuildContext;
    ///
    /// let ctx = BuildContext::default();
    /// let sink = ctx.output_registry().sink_for(TaskId::from("task-1"), 1);
    ///
    /// assert_eq!(sink.attempt(), 1);
    /// ```
    pub fn output_registry(&self) -> &Arc<OutputRegistry> {
        &self.output_registry
    }

    /// Replace the environment and return updated context.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::RunnerEnv;
    /// use solti_runner::BuildContext;
    ///
    /// let mut env = RunnerEnv::new();
    /// env.push("FOO", "bar");
    ///
    /// let ctx = BuildContext::default().with_env(env);
    /// assert_eq!(ctx.env().get("FOO"), Some("bar"));
    /// ```
    pub fn with_env(mut self, env: RunnerEnv) -> Self {
        self.env = env;
        self
    }

    /// Replace the metrics backend and return updated context.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::{BuildContext, RunnerType, noop_metrics};
    ///
    /// let ctx = BuildContext::default().with_metrics(noop_metrics());
    /// ctx.metrics().record_task_started(RunnerType::Subprocess);
    /// ```
    pub fn with_metrics(mut self, metrics: MetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }

    /// Replace the output registry and return updated context.
    ///
    /// ## Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use solti_runner::{BuildContext, OutputRegistry};
    ///
    /// let registry = Arc::new(OutputRegistry::new(128));
    /// let ctx = BuildContext::default().with_output_registry(registry.clone());
    ///
    /// assert!(Arc::ptr_eq(ctx.output_registry(), &registry));
    /// ```
    pub fn with_output_registry(mut self, registry: Arc<OutputRegistry>) -> Self {
        self.output_registry = registry;
        self
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self {
            env: RunnerEnv::default(),
            metrics: crate::metrics::noop_metrics(),
            output_registry: Arc::new(OutputRegistry::default()),
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

impl fmt::Display for BuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BuildContext(env_len={})", self.env.len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::BuildContext;
    use crate::OutputRegistry;

    use solti_model::{RunnerEnv, TaskId};

    #[test]
    fn default_build_context_has_empty_env_and_noop_metrics() {
        let ctx = BuildContext::default();
        assert_eq!(ctx.env().len(), 0);
    }

    #[test]
    fn new_uses_provided_env_and_metrics() {
        let mut env = RunnerEnv::new();
        env.push("FOO", "bar");
        env.push("BAZ", "qux");

        let metrics = crate::metrics::noop_metrics();
        let ctx = BuildContext::new(env.clone(), metrics);

        assert_eq!(ctx.env().len(), env.len());
        assert_eq!(ctx.env().get("FOO"), Some("bar"));
        assert_eq!(ctx.env().get("BAZ"), Some("qux"));
    }

    #[test]
    fn with_env_replaces_existing_env() {
        let mut env1 = RunnerEnv::new();
        env1.push("FOO", "one");

        let mut env2 = RunnerEnv::new();
        env2.push("BAR", "two");

        let metrics = crate::metrics::noop_metrics();
        let ctx = BuildContext::new(env1, metrics).with_env(env2.clone());

        assert_eq!(ctx.env().len(), env2.len());
        assert!(ctx.env().get("FOO").is_none());
        assert_eq!(ctx.env().get("BAR"), Some("two"));
    }

    #[test]
    fn with_metrics_replaces_backend() {
        let env = RunnerEnv::new();
        let metrics1 = crate::metrics::noop_metrics();
        let metrics2 = crate::metrics::noop_metrics();

        let ctx = BuildContext::new(env, metrics1).with_metrics(metrics2);

        ctx.metrics()
            .record_task_started(crate::RunnerType::Subprocess);
    }

    #[test]
    fn display_includes_env_length() {
        let mut env = RunnerEnv::new();
        env.push("FOO", "bar");

        let metrics = crate::metrics::noop_metrics();
        let ctx = BuildContext::new(env, metrics);

        let s = ctx.to_string();
        assert_eq!(s, "BuildContext(env_len=1)");
    }

    #[test]
    fn metrics_handle_can_be_cloned() {
        let ctx = BuildContext::default();
        let handle = ctx.metrics().clone();

        handle.record_task_started(crate::RunnerType::Subprocess);
        handle.record_task_completed(
            crate::RunnerType::Subprocess,
            crate::MetricOutcome::Success,
            100,
        );
    }
    #[test]
    fn default_build_context_has_an_empty_output_registry() {
        let ctx = BuildContext::default();
        assert_eq!(ctx.output_registry().active_channels(), 0);
    }

    #[test]
    fn with_output_registry_replaces_registry() {
        let custom = Arc::new(OutputRegistry::new(2048));
        let _ = custom.sink_for(TaskId::from("seed"), 1);

        let ctx = BuildContext::default().with_output_registry(custom.clone());

        assert_eq!(ctx.output_registry().active_channels(), 1);
        assert!(Arc::ptr_eq(ctx.output_registry(), &custom));
    }

    #[test]
    fn output_registry_handle_is_shared_via_arc() {
        let ctx = BuildContext::default();
        let task = TaskId::from("shared");
        let _sink = ctx.output_registry().sink_for(task.clone(), 1);

        let handle = Arc::clone(ctx.output_registry());
        assert!(handle.subscribe(&task).is_some());
    }
}
