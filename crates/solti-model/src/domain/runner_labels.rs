use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Structured key–value metadata based on [`BTreeMap`].
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunnerLabels(pub BTreeMap<String, String>);

impl RunnerLabels {
    /// Create an empty set of labels.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Returns `true` if no labels are present.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Insert or overwrite a label.
    ///
    /// Returns `self` for chaining.
    pub fn insert<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.insert(key.into(), val.into());
        self
    }

    /// Get the value for a key, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|s| s.as_str())
    }

    /// Iterate through all labels as `(&str, &str)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::RunnerLabels;

    #[test]
    fn new_is_empty() {
        let labels = RunnerLabels::new();
        assert!(labels.is_empty());
        assert!(labels.get("any").is_none());
    }

    #[test]
    fn insert_and_get() {
        let mut labels = RunnerLabels::new();
        labels.insert("region", "us-east-1");

        assert!(!labels.is_empty());
        assert_eq!(labels.get("region"), Some("us-east-1"));
        assert!(labels.get("zone").is_none());
    }

    #[test]
    fn insert_overwrites() {
        let mut labels = RunnerLabels::new();
        labels.insert("env", "dev");
        labels.insert("env", "prod");

        assert_eq!(labels.get("env"), Some("prod"));
    }

    #[test]
    fn insert_chaining() {
        let mut labels = RunnerLabels::new();
        labels.insert("a", "1").insert("b", "2");

        assert_eq!(labels.get("a"), Some("1"));
        assert_eq!(labels.get("b"), Some("2"));
    }

    #[test]
    fn iter_returns_sorted_pairs() {
        let mut labels = RunnerLabels::new();
        labels.insert("z", "last");
        labels.insert("a", "first");

        let pairs: Vec<_> = labels.iter().collect();
        assert_eq!(pairs, vec![("a", "first"), ("z", "last")]);
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let mut labels = RunnerLabels::new();
        labels.insert("runner-tag", "prod");

        let json = serde_json::to_string(&labels).unwrap();
        assert!(json.contains("\"runner-tag\":\"prod\""));

        let back: RunnerLabels = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("runner-tag"), Some("prod"));
    }
}
