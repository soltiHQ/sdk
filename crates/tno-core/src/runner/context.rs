use std::fmt;

use tno_model::TaskEnv;

use crate::metrics::MetricsHandle;

/// Shared build context passed to all runners.
#[derive(Clone)]
pub struct BuildContext {
    env: TaskEnv,
    metrics: MetricsHandle,
}

impl BuildContext {
    /// Create a new build context with the given params.
    pub fn new(env: TaskEnv, metrics: MetricsHandle) -> Self {
        Self { env, metrics }
    }

    /// Get a reference to the shared environment.
    pub fn env(&self) -> &TaskEnv {
        &self.env
    }

    /// Get a clonable handle to the metrics backend.
    pub fn metrics(&self) -> &MetricsHandle {
        &self.metrics
    }

    /// Replace the environment and return updated context.
    pub fn with_env(mut self, env: TaskEnv) -> Self {
        self.env = env;
        self
    }

    /// Replace the metrics backend and return unpdated context.
    pub fn with_metrics(mut self, metrics: MetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self {
            env: TaskEnv::default(),
            metrics: crate::metrics::noop_metrics(),
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
    use super::BuildContext;
    use tno_model::TaskEnv;

    #[test]
    fn default_build_context_has_empty_env_and_noop_metrics() {
        let ctx = BuildContext::default();
        assert_eq!(ctx.env().len(), 0);
    }

    #[test]
    fn new_uses_provided_env_and_metrics() {
        let mut env = TaskEnv::new();
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
        let mut env1 = TaskEnv::new();
        env1.push("FOO", "one");

        let mut env2 = TaskEnv::new();
        env2.push("BAR", "two");

        let metrics = crate::metrics::noop_metrics();
        let ctx = BuildContext::new(env1, metrics).with_env(env2.clone());

        assert_eq!(ctx.env().len(), env2.len());
        assert!(ctx.env().get("FOO").is_none());
        assert_eq!(ctx.env().get("BAR"), Some("two"));
    }

    #[test]
    fn with_metrics_replaces_backend() {
        let env = TaskEnv::new();
        let metrics1 = crate::metrics::noop_metrics();
        let metrics2 = crate::metrics::noop_metrics();

        let ctx = BuildContext::new(env, metrics1).with_metrics(metrics2);

        // metrics заменён (проверяем что не паникует)
        ctx.metrics().record_task_started("test");
    }

    #[test]
    fn display_includes_env_length() {
        let mut env = TaskEnv::new();
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

        handle.record_task_started("test");
        handle.record_task_completed("test", crate::TaskOutcome::Success, 100);
    }
}
