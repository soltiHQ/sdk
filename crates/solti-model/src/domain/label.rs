//! # Key-value metadata labels.
//!
//! [`Labels`] is an ordered map used for runner routing and task filtering.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Structured key–value metadata based on [`BTreeMap`].
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// Create an empty set of labels.
    #[inline]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Returns the number of labels.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if no labels are present.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Insert or overwrite a label.
    #[inline]
    pub fn insert<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.insert(key.into(), val.into());
        self
    }

    /// Get the value for a key, if present.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|s| s.as_str())
    }

    /// Check whether a key exists (regardless of its value).
    #[inline]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Iterate through all labels as `(&str, &str)` pairs.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl<'a> IntoIterator for &'a Labels {
    type Item = (&'a str, &'a str);
    type IntoIter = LabelsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        LabelsIter(self.0.iter())
    }
}

/// Iterator over `Labels` yielding `(&str, &str)` pairs.
pub struct LabelsIter<'a>(std::collections::btree_map::Iter<'a, String, String>);

impl<'a> Iterator for LabelsIter<'a> {
    type Item = (&'a str, &'a str);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for LabelsIter<'_> {}

#[cfg(test)]
mod tests {
    use super::Labels;

    #[test]
    fn new_is_empty() {
        let labels = Labels::new();
        assert!(labels.is_empty());
        assert_eq!(labels.len(), 0);
        assert!(labels.get("any").is_none());
    }

    #[test]
    fn insert_and_get() {
        let mut labels = Labels::new();
        labels.insert("region", "us-east-1");

        assert!(!labels.is_empty());
        assert_eq!(labels.len(), 1);
        assert_eq!(labels.get("region"), Some("us-east-1"));
        assert!(labels.get("zone").is_none());
    }

    #[test]
    fn insert_overwrites() {
        let mut labels = Labels::new();
        labels.insert("env", "dev");
        labels.insert("env", "prod");

        assert_eq!(labels.get("env"), Some("prod"));
    }

    #[test]
    fn insert_chaining() {
        let mut labels = Labels::new();
        labels.insert("a", "1").insert("b", "2");

        assert_eq!(labels.get("a"), Some("1"));
        assert_eq!(labels.get("b"), Some("2"));
    }

    #[test]
    fn iter_returns_sorted_pairs() {
        let mut labels = Labels::new();
        labels.insert("z", "last");
        labels.insert("a", "first");

        let pairs: Vec<_> = labels.iter().collect();
        assert_eq!(pairs, vec![("a", "first"), ("z", "last")]);
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let mut labels = Labels::new();
        labels.insert("runner-tag", "prod");

        let json = serde_json::to_string(&labels).unwrap();
        assert!(json.contains("\"runner-tag\":\"prod\""));

        let back: Labels = serde_json::from_str(&json).unwrap();
        assert_eq!(back.get("runner-tag"), Some("prod"));
    }
}
