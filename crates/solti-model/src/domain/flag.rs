//! # Boolean flag
//!
//! [`Flag`] provides named constructors for a serialized boolean value.

use serde::{Deserialize, Serialize};

/// Boolean flag with explicit enable/disable constructors.
///
/// ```rust
/// use solti_model::Flag;
///
/// let f = Flag::enabled();
/// assert!(f.is_enabled());
///
/// let f: Flag = false.into();
/// assert!(f.is_disabled());
///
/// let b: bool = f.into();
/// assert!(!b);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct Flag(bool);

impl Flag {
    /// Creates an enabled flag.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Flag;
    ///
    /// assert!(Flag::enabled().is_enabled());
    /// ```
    #[inline]
    pub const fn enabled() -> Self {
        Self(true)
    }

    /// Creates a disabled flag.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Flag;
    ///
    /// assert!(Flag::disabled().is_disabled());
    /// ```
    #[inline]
    pub const fn disabled() -> Self {
        Self(false)
    }

    /// Returns whether the flag is enabled.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        self.0
    }

    /// Returns whether the flag is disabled.
    #[inline]
    pub const fn is_disabled(&self) -> bool {
        !self.0
    }

    /// Returns the raw boolean value.
    #[inline]
    pub const fn value(&self) -> bool {
        self.0
    }
}

impl Default for Flag {
    #[inline]
    fn default() -> Self {
        Self::enabled()
    }
}

impl From<bool> for Flag {
    #[inline]
    fn from(b: bool) -> Self {
        Self(b)
    }
}

impl From<Flag> for bool {
    #[inline]
    fn from(f: Flag) -> Self {
        f.0
    }
}

#[cfg(test)]
mod tests {
    use super::Flag;

    #[test]
    fn constructors_default_and_bool_conversions_are_consistent() {
        for (flag, expected) in [
            (Flag::default(), true),
            (Flag::enabled(), true),
            (Flag::disabled(), false),
            (Flag::from(true), true),
            (Flag::from(false), false),
        ] {
            assert_eq!(flag.value(), expected);
            assert_eq!(flag.is_enabled(), expected);
            assert_eq!(flag.is_disabled(), !expected);
            assert_eq!(bool::from(flag), expected);
        }
    }

    #[test]
    fn serde_is_transparent() {
        for (flag, json) in [(Flag::enabled(), "true"), (Flag::disabled(), "false")] {
            assert_eq!(serde_json::to_string(&flag).unwrap(), json);
            assert_eq!(serde_json::from_str::<Flag>(json).unwrap(), flag);
        }
    }
}
