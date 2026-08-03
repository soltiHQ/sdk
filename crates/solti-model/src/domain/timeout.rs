//! # Attempt timeout
//!
//! [`Timeout`] is a positive millisecond value used by [`TaskSpec`](crate::TaskSpec).

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ModelError, ModelResult};

/// Timeout value in milliseconds.
///
/// ```
/// use solti_model::Timeout;
///
/// let timeout = Timeout::new(5_000).unwrap();
/// assert_eq!(timeout.as_millis(), 5_000);
///
/// let timeout = Timeout::new(10_000).unwrap();
/// assert_eq!(format!("{timeout}"), "10000ms");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct Timeout(NonZeroU64);

impl Timeout {
    /// Creates a timeout value.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `ms` is zero.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Timeout;
    ///
    /// let timeout = Timeout::new(5_000).unwrap();
    /// assert_eq!(timeout.as_millis(), 5_000);
    /// ```
    pub fn new(ms: u64) -> ModelResult<Self> {
        NonZeroU64::new(ms)
            .map(Self)
            .ok_or_else(|| ModelError::Invalid("timeout must be greater than zero".into()))
    }

    /// Returns the timeout in milliseconds.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Timeout;
    ///
    /// let timeout = Timeout::new(10_000).unwrap();
    /// assert_eq!(timeout.as_millis(), 10_000);
    /// ```
    pub const fn as_millis(&self) -> u64 {
        self.0.get()
    }
}

impl TryFrom<u64> for Timeout {
    type Error = ModelError;

    #[inline]
    fn try_from(ms: u64) -> Result<Self, Self::Error> {
        Self::new(ms)
    }
}

impl From<Timeout> for u64 {
    #[inline]
    fn from(t: Timeout) -> Self {
        t.as_millis()
    }
}

impl Serialize for Timeout {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.as_millis())
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Timeout {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::Timeout;

    #[test]
    fn exposes_milliseconds_conversions_display_and_ordering() {
        let timeout = Timeout::try_from(1_500).unwrap();
        assert_eq!(timeout.as_millis(), 1_500);
        assert_eq!(format!("{timeout}"), "1500ms");
        assert_eq!(u64::from(timeout), 1_500);

        let a = Timeout::new(100).unwrap();
        let b = Timeout::new(200).unwrap();
        assert!(a < b);
    }

    #[test]
    fn serde_is_transparent_and_validated() {
        let timeout = Timeout::new(5_000).unwrap();
        let json = serde_json::to_string(&timeout).unwrap();
        assert_eq!(json, "5000");
        assert_eq!(serde_json::from_str::<Timeout>(&json).unwrap(), timeout);
        assert!(Timeout::new(0).is_err());
        assert!(serde_json::from_str::<Timeout>("0").is_err());
    }
}
