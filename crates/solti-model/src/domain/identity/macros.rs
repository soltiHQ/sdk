//! Shared macro for `Arc<str>` backed identity newtypes.
//!
//! # Usage
//!
//! ```ignore
//! arc_str_newtype! {
//!     /// Docs for the type, rendered verbatim.
//!     #[doc = "..."]
//!     pub struct Slot;
//! }
//! ```

macro_rules! arc_str_newtype {
    (
        $(#[$meta:meta])*
        $vis:vis struct $ty:ident;
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $ty(std::sync::Arc<str>);

        impl $ty {
            /// Validate and create an identifier from a string slice.
            #[inline]
            pub fn new(s: impl AsRef<str>) -> $crate::ModelResult<Self> {
                let id = Self(std::sync::Arc::from(s.as_ref()));
                id.validate_format()?;
                Ok(id)
            }

            /// Access the underlying string slice.
            #[inline]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume the wrapper, returning the underlying `Arc<str>`.
            #[inline]
            pub fn into_inner(self) -> std::sync::Arc<str> {
                self.0
            }
        }

        impl std::str::FromStr for $ty {
            type Err = $crate::ModelError;

            #[inline]
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $ty {
            #[inline]
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $ty {
            #[inline]
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $ty {
            #[inline]
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $ty {
            #[inline]
            fn eq(&self, other: &str) -> bool {
                &*self.0 == other
            }
        }

        impl PartialEq<&str> for $ty {
            #[inline]
            fn eq(&self, other: &&str) -> bool {
                &*self.0 == *other
            }
        }

        impl PartialEq<String> for $ty {
            #[inline]
            fn eq(&self, other: &String) -> bool {
                &*self.0 == other.as_str()
            }
        }

        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            /// Deserialization validates: every identity type must define
            /// `validate_format`, and untrusted input cannot smuggle an id
            /// past the charset rules (ids leak into filesystem and cgroup paths).
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }
    };
}
