use serde::{Deserialize, Serialize};

use crate::KeyValue;

/// List of environment variables passed to the task.
///
/// Internally stored as a list of key–value pairs and serialized as a transparent array wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskEnv(pub Vec<KeyValue>);

impl TaskEnv {
    /// Create an empty environment.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    // Return len.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Create an environment containing a single key–value pair.
    pub fn single<K, V>(key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self(vec![KeyValue::new(key, value)])
    }

    /// Check if the environment is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate over all key–value pairs.
    pub fn iter(&self) -> impl Iterator<Item = &KeyValue> {
        self.0.iter()
    }

    /// Get the value for a key, returning the last matching entry.
    ///
    /// This allows simple override semantics when merging environments.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|kv| kv.key() == key)
            .map(|kv| kv.value())
    }

    /// Append a key–value pair to the environment.
    ///
    /// Later entries override earlier ones when queried via [`TaskEnv::get`].
    pub fn push<K, V>(&mut self, key: K, value: V)
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.push(KeyValue::new(key, value));
    }

    /// Merge two environments, where entries from `other` override earlier ones.
    ///
    /// The environments are combined by simple concatenation, allowing [`TaskEnv::get`] to resolve overrides naturally by scanning from the end.
    pub fn merged(&self, other: &TaskEnv) -> TaskEnv {
        let mut out = self.0.clone();
        out.extend(other.0.clone());
        TaskEnv(out)
    }
}

impl Default for TaskEnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::TaskEnv;

    #[test]
    fn env_new_is_empty() {
        let env = TaskEnv::new();
        assert_eq!(env.0.len(), 0);
        assert!(env.get("FOO").is_none());
    }

    #[test]
    fn env_single_creates_one_entry() {
        let env = TaskEnv::single("FOO", "bar");
        let items: Vec<_> = env.iter().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key(), "FOO");
        assert_eq!(items[0].value(), "bar");
        assert_eq!(env.get("FOO"), Some("bar"));
    }

    #[test]
    fn env_push_and_override_last_wins() {
        let mut env = TaskEnv::new();
        env.push("FOO", "one");
        env.push("BAR", "x");
        env.push("FOO", "two");

        assert_eq!(env.get("FOO"), Some("two"));
        assert_eq!(env.get("BAR"), Some("x"));
        assert!(env.get("BAZ").is_none());
    }

    #[test]
    fn env_merged_other_overrides_base() {
        let base = {
            let mut e = TaskEnv::new();
            e.push("FOO", "base");
            e.push("BAR", "bar");
            e
        };

        let other = {
            let mut e = TaskEnv::new();
            e.push("FOO", "override");
            e.push("BAZ", "baz");
            e
        };

        let merged = base.merged(&other);

        assert_eq!(merged.get("FOO"), Some("override"));
        assert_eq!(merged.get("BAR"), Some("bar"));
        assert_eq!(merged.get("BAZ"), Some("baz"));
    }

    #[test]
    fn serde_transparent_roundtrip_json() {
        let mut env = TaskEnv::new();
        env.push("FOO", "bar");
        env.push("BAZ", "qux");

        let json = serde_json::to_string(&env).unwrap();
        assert!(json.starts_with('['));
        assert!(json.contains("\"key\":\"FOO\""));
        assert!(json.contains("\"value\":\"bar\""));

        let back: TaskEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("FOO"), Some("bar"));
        assert_eq!(back.get("BAZ"), Some("qux"));
    }
}
