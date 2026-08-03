//! # Task environment
//!
//! [`TaskEnv`] is an ordered list attached to a task workload.
//! Duplicate keys are preserved.
//! Lookup uses the last matching value.
//! Keys and values are stored without validation.

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
    /// Creates an environment with one entry.
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
    fn constructors_iteration_and_lookup_preserve_last_value() {
        let mut env = TaskEnv::new();
        assert!(env.is_empty());

        env.push("FOO", "one");
        env.push("BAR", "x");
        env.push("FOO", "two");

        assert_eq!(env.len(), 3);
        assert_eq!(env.iter().next().unwrap().key(), "FOO");
        assert_eq!(env.get("FOO"), Some("two"));
        assert_eq!(env.get("BAR"), Some("x"));
        assert!(env.get("BAZ").is_none());

        let single = TaskEnv::single("ONLY", "value");
        assert_eq!(single.len(), 1);
        assert_eq!(single.get("ONLY"), Some("value"));
    }

    #[test]
    fn serde_is_transparent_and_preserves_entries() {
        let mut env = TaskEnv::new();
        env.push("FOO", "bar");
        env.push("BAZ", "qux");

        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(
            json,
            r#"[{"key":"FOO","value":"bar"},{"key":"BAZ","value":"qux"}]"#
        );
        let back: TaskEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
    }
}
