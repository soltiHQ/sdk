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
