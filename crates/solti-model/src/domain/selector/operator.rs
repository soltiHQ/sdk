//! Selector operators.
//!
//! [`SelectorOperator`] defines comparison operators for label selection.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Set-based operator for [`super::SelectorRequirement`].
///
/// | Operator       | Semantics                               |
/// |----------------|-----------------------------------------|
/// | `In`           | label value is one of `values`          |
/// | `NotIn`        | label value is not one of `values`      |
/// | `Exists`       | label key is present (values ignored)   |
/// | `DoesNotExist` | label key is absent  (values ignored)   |
///
/// ## Example
///
/// ```
/// use solti_model::SelectorOperator;
///
/// assert_eq!(SelectorOperator::DoesNotExist.to_string(), "DoesNotExist");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SelectorOperator {
    /// Label value must be one of `values`.
    In,
    /// Label value must NOT be one of `values`.
    NotIn,
    /// Label key must exist (values are ignored).
    Exists,
    /// Label key must NOT exist (values are ignored).
    DoesNotExist,
}

impl fmt::Display for SelectorOperator {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::In => f.write_str("In"),
            Self::NotIn => f.write_str("NotIn"),
            Self::Exists => f.write_str("Exists"),
            Self::DoesNotExist => f.write_str("DoesNotExist"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(SelectorOperator::In.to_string(), "In");
        assert_eq!(SelectorOperator::NotIn.to_string(), "NotIn");
        assert_eq!(SelectorOperator::Exists.to_string(), "Exists");
        assert_eq!(SelectorOperator::DoesNotExist.to_string(), "DoesNotExist");
    }

    #[test]
    fn serde_roundtrip() {
        for op in [
            SelectorOperator::In,
            SelectorOperator::NotIn,
            SelectorOperator::Exists,
            SelectorOperator::DoesNotExist,
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let back: SelectorOperator = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op);
        }
    }
}
