//! # Runner-level environment variables.
//!
//! [`RunnerEnv`] provides base environment variables shared across all tasks on a runner.

use super::macros::env_newtype;

env_newtype! {
    /// Environment variables injected by the runner.
    ///
    /// ```text
    ///  RunnerEnv (runner-level defaults)
    ///  ┌───────────────────────────────┐
    ///  │  PATH=/safe/bin               │  ← enforced by runner
    ///  │  RUNNER_NAME=prod-01          │
    ///  │  FOO=from-runner              │  ← overrides task FOO
    ///  └───────────────────────────────┘
    /// ```
    ///
    pub struct RunnerEnv;
}

#[cfg(test)]
mod tests {
    use super::RunnerEnv;

    #[test]
    fn new_is_empty() {
        let env = RunnerEnv::new();

        assert_eq!(env.len(), 0);
        assert!(env.is_empty());
        assert!(env.get("FOO").is_none());
    }

    #[test]
    fn push_and_get() {
        let mut env = RunnerEnv::new();
        env.push("PATH", "/safe/bin");
        env.push("RUNNER", "prod-01");

        assert_eq!(env.len(), 2);
        assert_eq!(env.get("PATH"), Some("/safe/bin"));
        assert_eq!(env.get("RUNNER"), Some("prod-01"));
    }

    #[test]
    fn last_wins() {
        let mut env = RunnerEnv::new();
        env.push("FOO", "first");
        env.push("FOO", "second");

        assert_eq!(env.get("FOO"), Some("second"));
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let mut env = RunnerEnv::new();
        env.push("K", "V");

        let json = serde_json::to_string(&env).unwrap();
        let back: RunnerEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("K"), Some("V"));
    }
}
