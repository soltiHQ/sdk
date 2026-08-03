//! # Labels
//!
//! [`Labels`] is a key-sorted map with Kubernetes label validation.
//! Insertion and direct deserialization do not validate entries.
//! Call [`Labels::validate`] at an input boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelResult, validation};

/// Structured key-value metadata based on [`BTreeMap`].
///
/// Iteration order is stable because labels are stored in key order.
///
/// ## Example
///
/// ```
/// use solti_model::Labels;
///
/// let mut labels = Labels::new();
/// labels.insert("zone", "eu");
/// labels.insert("gpu", "h100");
///
/// assert_eq!(labels.get("zone"), Some("eu"));
/// assert!(labels.contains_key("gpu"));
/// ```
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::labels"))]
#[serde(transparent)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// Creates an empty label map.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Labels;
    ///
    /// let labels = Labels::new();
    /// assert!(labels.is_empty());
    /// ```
    #[inline]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Returns the number of labels.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the map is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Inserts or replaces a label.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Labels;
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("tier", "dev").insert("tier", "prod");
    ///
    /// assert_eq!(labels.get("tier"), Some("prod"));
    /// ```
    #[inline]
    pub fn insert<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.insert(key.into(), val.into());
        self
    }

    /// Returns the value for a key.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|s| s.as_str())
    }

    /// Returns whether a key exists.
    #[inline]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Iterates over labels in key order.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Labels;
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("b", "2");
    /// labels.insert("a", "1");
    ///
    /// let pairs: Vec<_> = labels.iter().collect();
    /// assert_eq!(pairs, vec![("a", "1"), ("b", "2")]);
    /// ```
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Validates every label.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ModelError::Invalid`].
    pub fn validate(&self) -> ModelResult<()> {
        for (key, value) in &self.0 {
            validation::validate_qualified_name("label key", key)?;
            validation::validate_label_value("label value", value)?;
        }
        Ok(())
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
    fn insertion_lookup_overwrite_and_iteration_are_deterministic() {
        let mut labels = Labels::new();
        assert!(labels.is_empty());
        assert_eq!(labels.len(), 0);

        labels.insert("z", "last").insert("a", "first");
        labels.insert("env", "dev");
        labels.insert("env", "prod");

        assert_eq!(labels.len(), 3);
        assert_eq!(labels.get("env"), Some("prod"));
        assert_eq!(
            labels.iter().collect::<Vec<_>>(),
            vec![("a", "first"), ("env", "prod"), ("z", "last")]
        );
    }

    #[test]
    fn serde_is_transparent() {
        let mut labels = Labels::new();
        labels.insert("runner-tag", "prod");

        let json = serde_json::to_string(&labels).unwrap();
        assert_eq!(json, r#"{"runner-tag":"prod"}"#);
        let back: Labels = serde_json::from_str(&json).unwrap();
        assert_eq!(back, labels);
    }

    #[test]
    fn validation_uses_kubernetes_label_rules() {
        let mut labels = Labels::new();
        labels
            .insert("app.kubernetes.io/name", "solti_agent-1")
            .insert("empty", "");
        labels.validate().unwrap();

        let mut invalid_key = Labels::new();
        invalid_key.insert("example.io/bad key", "value");
        assert!(invalid_key.validate().is_err());

        let mut invalid_value = Labels::new();
        invalid_value.insert("example.io/name", "-value");
        assert!(invalid_value.validate().is_err());
    }
}
