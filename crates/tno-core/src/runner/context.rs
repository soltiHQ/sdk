use std::fmt;

use tno_model::Env;

/// Shared build context passed to all runners.
#[derive(Debug, Clone)]
pub struct BuildContext {
    env: Env,
}

impl BuildContext {
    /// Create a new build context with the given params.
    pub fn new(env: Env) -> Self {
        Self { env }
    }

    /// Get a reference to the shared environment.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Replace the environment and return updated context.
    pub fn with_env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }
}

impl Default for BuildContext {
    fn default() -> Self {
        Self {
            env: Env::default(),
        }
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
    use tno_model::Env;

    #[test]
    fn default_build_context_has_empty_env() {
        let ctx = BuildContext::default();
        assert_eq!(ctx.env().len(), 0);
    }

    #[test]
    fn new_uses_provided_env() {
        let mut env = Env::new();
        env.push("FOO", "bar");
        env.push("BAZ", "qux");

        let ctx = BuildContext::new(env.clone());

        assert_eq!(ctx.env().len(), env.len());
        assert_eq!(ctx.env().get("FOO"), Some("bar"));
        assert_eq!(ctx.env().get("BAZ"), Some("qux"));
    }

    #[test]
    fn with_env_replaces_existing_env() {
        let mut env1 = Env::new();
        env1.push("FOO", "one");

        let mut env2 = Env::new();
        env2.push("BAR", "two");

        let ctx = BuildContext::new(env1).with_env(env2.clone());

        assert_eq!(ctx.env().len(), env2.len());
        assert!(ctx.env().get("FOO").is_none());
        assert_eq!(ctx.env().get("BAR"), Some("two"));
    }

    #[test]
    fn display_includes_env_length() {
        let mut env = Env::new();
        env.push("FOO", "bar");
        let ctx = BuildContext::new(env);

        let s = ctx.to_string();
        assert_eq!(s, "BuildContext(env_len=1)");
    }
}
