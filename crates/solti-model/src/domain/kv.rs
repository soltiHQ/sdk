//! # Key-value pair
//!
//! [`KeyValue`] stores one ordered key-value entry.
//! It does not apply key or value format validation.

use serde::{Deserialize, Serialize};

/// Key-value pair used for environment variables or generic metadata.
///
/// ## Example
///
/// ```
/// use solti_model::KeyValue;
///
/// let kv = KeyValue::new("APP_MODE", "batch");
/// assert_eq!(kv.key(), "APP_MODE");
/// assert_eq!(kv.value(), "batch");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyValue {
    /// Name of the variable or key.
    key: String,
    /// Value associated with the key.
    value: String,
}

impl KeyValue {
    /// Creates a key-value pair.
    #[inline]
    pub fn new<K, V>(key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Returns the key.
    #[inline]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the value.
    #[inline]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl From<(String, String)> for KeyValue {
    #[inline]
    fn from((key, value): (String, String)) -> Self {
        Self { key, value }
    }
}

impl From<(&str, &str)> for KeyValue {
    #[inline]
    fn from((key, value): (&str, &str)) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyValue;

    #[test]
    fn constructors_and_equality_preserve_key_and_value() {
        let expected = KeyValue::new("FOO", "bar");
        let from_str: KeyValue = ("FOO", "bar").into();
        let from_string: KeyValue = (String::from("FOO"), String::from("bar")).into();

        assert_eq!(expected.key(), "FOO");
        assert_eq!(expected.value(), "bar");
        assert_eq!(from_str, expected);
        assert_eq!(from_string, expected);
        assert_ne!(KeyValue::new("FOO", "baz"), expected);
    }

    #[test]
    fn serde_roundtrip_preserves_fields() {
        let kv = KeyValue::new("FOO", "bar");
        let json = serde_json::to_string(&kv).unwrap();
        assert_eq!(json, r#"{"key":"FOO","value":"bar"}"#);
        let back: KeyValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kv);
    }
}
