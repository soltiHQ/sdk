use serde::{Deserialize, Serialize};

/// Key–value pair used for environment variables or generic metadata.
///
/// Both fields are plain UTF-8 strings with no validation applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    /// Name of the variable or key.
    key: String,
    /// Value associated with the key.
    value: String,
}

impl KeyValue {
    /// Create a new key–value pair.
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

    /// Get the key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Get the value.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl From<(String, String)> for KeyValue {
    fn from((key, value): (String, String)) -> Self {
        Self { key, value }
    }
}

impl From<(&str, &str)> for KeyValue {
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
    fn new_sets_key_and_value() {
        let kv = KeyValue::new("FOO", "bar");
        assert_eq!(kv.key(), "FOO");
        assert_eq!(kv.value(), "bar");
    }

    #[test]
    fn from_str_tuple_creates_keyvalue() {
        let kv: KeyValue = ("FOO", "bar").into();
        assert_eq!(kv.key(), "FOO");
        assert_eq!(kv.value(), "bar");
    }

    #[test]
    fn from_string_tuple_creates_keyvalue() {
        let kv: KeyValue = (String::from("FOO"), String::from("bar")).into();
        assert_eq!(kv.key(), "FOO");
        assert_eq!(kv.value(), "bar");
    }

    #[test]
    fn equality_works_for_same_key_and_value() {
        let a = KeyValue::new("FOO", "bar");
        let b = KeyValue::new("FOO", "bar");
        let c = KeyValue::new("FOO", "baz");

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn serde_roundtrip_json() {
        let kv = KeyValue::new("FOO", "bar");
        let json = serde_json::to_string(&kv).unwrap();
        // due to rename_all = "camelCase"
        assert!(json.contains("\"key\":\"FOO\""));
        assert!(json.contains("\"value\":\"bar\""));

        let back: KeyValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.key(), "FOO");
        assert_eq!(back.value(), "bar");
    }
}
