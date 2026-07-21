//! Runner-owned environment configuration and task environment merging.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use solti_model::{KeyValue, TaskEnv};

/// Environment variables injected by a runner.
///
/// Values are kept in insertion order with last-value-wins lookup semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerEnv(Vec<KeyValue>);

impl RunnerEnv {
    /// Create an empty runner environment.
    #[inline]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Return the number of entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return whether the environment is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate through entries in insertion order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &KeyValue> {
        self.0.iter()
    }

    /// Get the last value for a key.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|entry| entry.key() == key)
            .map(KeyValue::value)
    }

    /// Append an environment entry.
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
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> IntoIterator for &'a RunnerEnv {
    type Item = &'a KeyValue;
    type IntoIter = std::slice::Iter<'a, KeyValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Merge task and runner environments into a sorted process environment.
///
/// Runner values override task values. Duplicate entries within either input
/// use their last value.
pub fn merge_env(task: &TaskEnv, runner: &RunnerEnv) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    for entry in runner.into_iter().rev().chain(task.into_iter().rev()) {
        merged
            .entry(entry.key().to_owned())
            .or_insert_with(|| entry.value().to_owned());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_values_win_and_last_value_wins() {
        let mut task = TaskEnv::new();
        task.push("PATH", "/task/bin");
        task.push("TASK_ONLY", "yes");

        let mut runner = RunnerEnv::new();
        runner.push("PATH", "/first");
        runner.push("PATH", "/runner/bin");

        let merged = merge_env(&task, &runner);
        assert_eq!(merged["PATH"], "/runner/bin");
        assert_eq!(merged["TASK_ONLY"], "yes");
    }

    #[test]
    fn serde_roundtrip_preserves_entries() {
        let mut env = RunnerEnv::new();
        env.push("A", "1");
        env.push("A", "2");

        let json = serde_json::to_string(&env).unwrap();
        let back: RunnerEnv = serde_json::from_str(&json).unwrap();

        assert_eq!(back, env);
        assert_eq!(back.get("A"), Some("2"));
    }
}
