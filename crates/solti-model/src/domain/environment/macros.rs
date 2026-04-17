//! Shared macro for `Vec<KeyValue>` backed environment newtypes.
//!
//! ```ignore
//! env_newtype! {
//!     /// Type-specific docs go here, rendered verbatim.
//!     pub struct TaskEnv;
//! }
//! ```

macro_rules! env_newtype {
    (
        $(#[$meta:meta])*
        $vis:vis struct $ty:ident;
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        $vis struct $ty(Vec<$crate::KeyValue>);

        impl $ty {
            /// Create an empty environment.
            #[inline]
            pub fn new() -> Self {
                Self(Vec::new())
            }

            /// Returns the number of key–value pairs.
            #[inline]
            pub fn len(&self) -> usize {
                self.0.len()
            }

            /// Returns `true` if the environment has no entries.
            #[inline]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }

            /// Iterate over all key–value pairs in insertion order.
            #[inline]
            pub fn iter(&self) -> impl Iterator<Item = &$crate::KeyValue> {
                self.0.iter()
            }

            /// Get the value for a key, returning the **last** matching entry (last-wins override semantics).
            #[inline]
            pub fn get(&self, key: &str) -> Option<&str> {
                self.0
                    .iter()
                    .rev()
                    .find(|kv| kv.key() == key)
                    .map(|kv| kv.value())
            }

            /// Append a key–value pair.
            #[inline]
            pub fn push<K, V>(&mut self, key: K, value: V)
            where
                K: Into<String>,
                V: Into<String>,
            {
                self.0.push($crate::KeyValue::new(key, value));
            }
        }

        impl Default for $ty {
            #[inline]
            fn default() -> Self {
                Self::new()
            }
        }

        impl<'a> IntoIterator for &'a $ty {
            type Item = &'a $crate::KeyValue;
            type IntoIter = std::slice::Iter<'a, $crate::KeyValue>;

            #[inline]
            fn into_iter(self) -> Self::IntoIter {
                self.0.iter()
            }
        }
    };
}

pub(super) use env_newtype;
