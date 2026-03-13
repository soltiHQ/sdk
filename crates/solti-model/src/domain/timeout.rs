use std::fmt;

use serde::{Deserialize, Serialize};

/// Timeout value in milliseconds.
///
/// Used in task specifications and controller rules where an explicit
/// time limit is required. Wraps a `u64` to prevent accidental
/// mix-ups with other integer fields.
///
/// ```
/// use solti_model::TimeoutMs;
///
/// let timeout = TimeoutMs::new(5_000);
/// assert_eq!(timeout.as_millis(), 5_000);
///
/// // From u64
/// let timeout: TimeoutMs = 10_000.into();
/// assert_eq!(format!("{timeout}"), "10000ms");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimeoutMs(u64);

impl TimeoutMs {
    /// Create a new timeout value.
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    /// Get the timeout in milliseconds.
    pub const fn as_millis(&self) -> u64 {
        self.0
    }
}

impl From<u64> for TimeoutMs {
    fn from(ms: u64) -> Self {
        Self(ms)
    }
}

impl From<TimeoutMs> for u64 {
    fn from(t: TimeoutMs) -> Self {
        t.0
    }
}

impl fmt::Display for TimeoutMs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::TimeoutMs;

    #[test]
    fn new_and_as_millis() {
        let t = TimeoutMs::new(3_000);
        assert_eq!(t.as_millis(), 3_000);
    }

    #[test]
    fn from_u64_and_into() {
        let t: TimeoutMs = 5_000.into();
        let v: u64 = t.into();
        assert_eq!(v, 5_000);
    }

    #[test]
    fn display() {
        let t = TimeoutMs::new(1_500);
        assert_eq!(format!("{t}"), "1500ms");
    }

    #[test]
    fn ordering() {
        let a = TimeoutMs::new(100);
        let b = TimeoutMs::new(200);
        assert!(a < b);
    }

    #[test]
    fn serde_transparent() {
        let t = TimeoutMs::new(5_000);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "5000");

        let back: TimeoutMs = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }
}
