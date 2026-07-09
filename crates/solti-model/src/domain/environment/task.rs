//! Task-level environment variables.
//!
//! [`TaskEnv`] is an ordered list of key-value pairs attached to a single task spec.

env_newtype! {
    /// Environment variables passed to a task at submission time.
    ///
    /// Duplicate keys are allowed. Lookup uses last-value-wins semantics.
    ///
    /// ```
    /// use solti_model::TaskEnv;
    ///
    /// let mut env = TaskEnv::new();
    /// env.push("APP_MODE", "dev");
    /// env.push("APP_MODE", "prod");
    ///
    /// assert_eq!(env.get("APP_MODE"), Some("prod"));
    /// ```
    pub struct TaskEnv;
}

impl TaskEnv {
    /// Create an environment containing a single key-value pair.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::TaskEnv;
    ///
    /// let env = TaskEnv::single("APP_MODE", "batch");
    /// assert_eq!(env.get("APP_MODE"), Some("batch"));
    /// ```
    #[inline]
    pub fn single<K, V>(key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let mut env = Self::new();
        env.push(key, value);
        env
    }
}

#[cfg(test)]
mod tests {
    use super::TaskEnv;

    #[test]
    fn env_new_is_empty() {
        let env = TaskEnv::new();

        assert_eq!(env.len(), 0);
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
