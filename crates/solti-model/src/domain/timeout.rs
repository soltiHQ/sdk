//! # Per-attempt timeout.
//!
//! [`Timeout`] is a validated wrapper over milliseconds, used in [`TaskSpec`](crate::TaskSpec).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Timeout value in milliseconds.
///
/// ```
/// use solti_model::Timeout;
///
/// let timeout = Timeout::new(5_000);
/// assert_eq!(timeout.as_millis(), 5_000);
///
/// let timeout: Timeout = 10_000.into();
/// assert_eq!(format!("{timeout}"), "10000ms");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timeout(u64);

impl Timeout {
    /// Create a new timeout value.
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    /// Get the timeout in milliseconds.
    pub const fn as_millis(&self) -> u64 {
        self.0
    }
}

impl From<u64> for Timeout {
    #[inline]
    fn from(ms: u64) -> Self {
        Self(ms)
    }
}

impl From<Timeout> for u64 {
    #[inline]
    fn from(t: Timeout) -> Self {
        t.0
    }
}

impl fmt::Display for Timeout {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Timeout;

    #[test]
    fn new_and_as_millis() {
        let t = Timeout::new(3_000);
        assert_eq!(t.as_millis(), 3_000);
    }

    #[test]
    fn from_u64_and_into() {
        let t: Timeout = 5_000.into();
        let v: u64 = t.into();
        assert_eq!(v, 5_000);
    }

    #[test]
    fn display() {
        let t = Timeout::new(1_500);
        assert_eq!(format!("{t}"), "1500ms");
    }

    #[test]
    fn ordering() {
        let a = Timeout::new(100);
        let b = Timeout::new(200);
        assert!(a < b);
    }

    #[test]
    fn serde_transparent() {
        let t = Timeout::new(5_000);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "5000");

        let back: Timeout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }
}
