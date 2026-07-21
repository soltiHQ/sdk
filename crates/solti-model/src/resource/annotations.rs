//! Free-form resource annotations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelError, ModelResult, validation};

const ANNOTATIONS_MAX_TOTAL_BYTES: usize = 256 * 1024;

/// Ordered, free-form metadata attached to a resource.
///
/// Annotations are kept distinct from labels because they are descriptive data,
/// not selectors used for routing or filtering.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Annotations(BTreeMap<String, String>);

impl Annotations {
    /// Create an empty annotation map.
    #[inline]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Return the number of annotations.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return `true` when no annotations are present.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Insert or replace an annotation.
    #[inline]
    pub fn insert<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        self.0.insert(key.into(), value.into());
        self
    }

    /// Get an annotation value.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Iterate through annotations in key order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Validate annotation keys and the Kubernetes total annotation size limit.
    ///
    /// Annotation values remain arbitrary UTF-8 strings.
    pub fn validate(&self) -> ModelResult<()> {
        let mut total_bytes = 0_usize;
        for (key, value) in &self.0 {
            validation::validate_qualified_name("annotation key", key)?;
            total_bytes = total_bytes
                .saturating_add(key.len())
                .saturating_add(value.len());
        }
        if total_bytes > ANNOTATIONS_MAX_TOTAL_BYTES {
            return Err(ModelError::Invalid(
                format!(
                    "annotations total size {total_bytes} bytes exceeds max {ANNOTATIONS_MAX_TOTAL_BYTES}"
                )
                .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_preserves_entries() {
        let mut annotations = Annotations::new();
        annotations.insert("example.io/note", "kept verbatim");

        let json = serde_json::to_string(&annotations).unwrap();
        let back: Annotations = serde_json::from_str(&json).unwrap();

        assert_eq!(back, annotations);
    }

    #[test]
    fn validate_accepts_arbitrary_values() {
        let mut annotations = Annotations::new();
        annotations.insert("example.io/note", "spaces, JSON: {\"ok\":true}");

        annotations.validate().unwrap();
    }

    #[test]
    fn validate_rejects_invalid_key() {
        let mut annotations = Annotations::new();
        annotations.insert("example.io/bad key", "value");

        assert!(annotations.validate().is_err());
    }

    #[test]
    fn validate_rejects_total_size_above_kubernetes_limit() {
        let mut annotations = Annotations::new();
        annotations.insert("example.io/data", "x".repeat(ANNOTATIONS_MAX_TOTAL_BYTES));

        assert!(annotations.validate().is_err());
    }
}
