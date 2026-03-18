use serde::{Deserialize, Serialize};

use crate::KeyValue;

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
/// Runner environment takes **precedence** over task environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerEnv(Vec<KeyValue>);

impl RunnerEnv {
    /// Create an empty environment.
    #[inline]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Returns the number of key–value pairs in the environment.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Check if the environment is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate over all key–value pairs.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &KeyValue> {
        self.0.iter()
    }

    /// Get the value for a key, returning the last matching entry.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|kv| kv.key() == key)
            .map(|kv| kv.value())
    }

    /// Append a key–value pair to the environment.
    #[inline]
    pub fn push<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.push(KeyValue::new(key, value));
    }
}

impl Default for RunnerEnv {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> IntoIterator for &'a RunnerEnv {
    type Item = &'a KeyValue;
    type IntoIter = std::slice::Iter<'a, KeyValue>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
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
