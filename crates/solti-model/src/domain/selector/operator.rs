//! # Selector operator
//!
//! [`SelectorOperator`] defines set-based label comparisons.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Set-based operator for [`super::SelectorRequirement`].
///
/// | Operator       | Values    | Match                                             |
/// |----------------|-----------|---------------------------------------------------|
/// | `In`           | non-empty | key exists and value is in `values`               |
/// | `NotIn`        | non-empty | key is absent or value is not in `values`         |
/// | `Exists`       | empty     | key exists                                        |
/// | `DoesNotExist` | empty     | key is absent                                     |
///
/// ## Example
///
/// ```
/// use solti_model::SelectorOperator;
///
/// assert_eq!(SelectorOperator::DoesNotExist.to_string(), "DoesNotExist");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum SelectorOperator {
    /// Label value must be one of `values`.
    In,
    /// Label is absent or its value is not in `values`.
    NotIn,
    /// Label key must exist.
    Exists,
    /// Label key must not exist.
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
    fn display_and_serde_use_operator_names() {
        for (operator, name) in [
            (SelectorOperator::In, "In"),
            (SelectorOperator::NotIn, "NotIn"),
            (SelectorOperator::Exists, "Exists"),
            (SelectorOperator::DoesNotExist, "DoesNotExist"),
        ] {
            assert_eq!(operator.to_string(), name);
            let json = serde_json::to_string(&operator).unwrap();
            let back: SelectorOperator = serde_json::from_str(&json).unwrap();
            assert_eq!(back, operator);
        }
    }
}
