use serde::{Deserialize, Serialize};

/// Universal boolean flag with explicit enable/disable semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Flag(bool);

impl Flag {
    /// Create an enabled flag.
    pub const fn enabled() -> Self {
        Self(true)
    }

    /// Create a disabled flag.
    pub const fn disabled() -> Self {
        Self(false)
    }

    /// Check if the flag is enabled.
    pub const fn is_enabled(&self) -> bool {
        self.0
    }

    /// Check if the flag is disabled.
    pub const fn is_disabled(&self) -> bool {
        !self.0
    }

    /// Get the raw boolean value.
    pub const fn value(&self) -> bool {
        self.0
    }
}

impl Default for Flag {
    fn default() -> Self {
        Self::enabled()
    }
}

impl From<bool> for Flag {
    fn from(b: bool) -> Self {
        Self(b)
    }
}

impl From<Flag> for bool {
    fn from(f: Flag) -> Self {
        f.0
    }
}

#[cfg(test)]
mod tests {
    use super::Flag;

    #[test]
    fn default_is_enabled() {
        let f = Flag::default();
        assert!(f.is_enabled());
        assert!(!f.is_disabled());
        assert!(f.value());
    }

    #[test]
    fn enabled_and_disabled_constructors_work() {
        let e = Flag::enabled();
        let d = Flag::disabled();

        assert!(e.is_enabled());
        assert!(!e.is_disabled());

        assert!(!d.is_enabled());
        assert!(d.is_disabled());
    }

    #[test]
    fn from_bool_and_into_bool() {
        let f_true: Flag = true.into();
        let f_false: Flag = false.into();

        assert!(f_true.is_enabled());
        assert!(f_false.is_disabled());

        let b1: bool = f_true.into();
        let b2: bool = f_false.into();

        assert!(b1);
        assert!(!b2);
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let f = Flag::disabled();
        let json = serde_json::to_string(&f).unwrap();

        assert_eq!(json, "false");
        let back: Flag = serde_json::from_str(&json).unwrap();
        assert!(back.is_disabled());
    }
}
