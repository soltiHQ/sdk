//! # Runner environment
//!
//! [`RunnerEnv`] stores environment values owned by a runner.
//! [`merge_env`] combines them with task values.
//!
//! ## Flow
//!
//! ```text
//! TaskEnv ──────┐
//!               ├── merge_env ──▶ sorted process environment
//! RunnerEnv ────┘
//!      ▲
//!      └── wins on duplicate keys
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use solti_model::{KeyValue, TaskEnv};

/// Environment variables injected by a runner.
///
/// Entries remain in insertion order.
/// The last entry wins when a key occurs more than once.
///
/// ## Example
///
/// ```
/// use solti_runner::RunnerEnv;
///
/// let mut env = RunnerEnv::new();
/// env.push("PATH", "/usr/bin");
/// env.push("PATH", "/opt/bin");
///
/// assert_eq!(env.get("PATH"), Some("/opt/bin"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerEnv(Vec<KeyValue>);

impl RunnerEnv {
    /// Creates an empty environment.
    #[inline]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Returns the number of entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` when the environment has no entries.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over entries in insertion order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &KeyValue> {
        self.0.iter()
    }

    /// Returns the last value stored for `key`.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|entry| entry.key() == key)
            .map(KeyValue::value)
    }

    /// Appends an environment entry.
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
/// Runner values override task values.
/// The last value wins within each input.
/// The returned [`BTreeMap`] sorts entries by key.
///
/// ## Example
///
/// ```
/// use solti_model::TaskEnv;
/// use solti_runner::{RunnerEnv, merge_env};
///
/// let mut task = TaskEnv::new();
/// task.push("PATH", "/task/bin");
///
/// let mut runner = RunnerEnv::new();
/// runner.push("PATH", "/runner/bin");
///
/// let merged = merge_env(&task, &runner);
/// assert_eq!(merged["PATH"], "/runner/bin");
/// ```
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
        task.push("PATH", "/task/first");
        task.push("PATH", "/task/last");
        task.push("TASK_ONLY", "first");
        task.push("TASK_ONLY", "last");

        let mut runner = RunnerEnv::new();
        runner.push("PATH", "/first");
        runner.push("PATH", "/runner/bin");
        runner.push("RUNNER_ONLY", "yes");

        let merged = merge_env(&task, &runner);
        assert_eq!(merged["PATH"], "/runner/bin");
        assert_eq!(merged["TASK_ONLY"], "last");
        assert_eq!(merged["RUNNER_ONLY"], "yes");
        assert_eq!(
            merged.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["PATH", "RUNNER_ONLY", "TASK_ONLY"]
        );
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
