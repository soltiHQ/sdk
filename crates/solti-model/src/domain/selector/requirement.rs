//! # Selector requirement.
//!
//! [`SelectorRequirement`] is a single label constraint used in [`RunnerSelector`](crate::RunnerSelector).

use serde::{Deserialize, Serialize};

use super::SelectorOperator;

/// Single set-based requirement for label matching.
///
/// ```text
///  { key: "gpu", operator: In, values: ["a100", "h100"] } ⇒ runner must have label gpu ∈ {"a100", "h100"}
///
///  { key: "zone", operator: Exists, values: [] } ⇒ runner must have label  zone  (any value)
/// ```
///
/// Used inside [`super::RunnerSelector::match_expressions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectorRequirement {
    /// Label key to evaluate.
    pub key: String,
    /// Comparison operator.
    pub operator: SelectorOperator,
    /// Values for `In` / `NotIn`.
    /// Must be empty for `Exists` / `DoesNotExist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

impl SelectorRequirement {
    /// Validate structural invariants.
    ///
    /// - `key` must not be empty
    /// - `In`/`NotIn` must have non-empty `values`
    /// - `Exists`/`DoesNotExist` must have empty `values`
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        use std::borrow::Cow;

        if self.key.is_empty() {
            return Err(crate::ModelError::Invalid(Cow::Borrowed(
                "selector requirement key must not be empty",
            )));
        }
        match self.operator {
            SelectorOperator::In | SelectorOperator::NotIn => {
                if self.values.is_empty() {
                    return Err(crate::ModelError::Invalid(Cow::Owned(format!(
                        "selector requirement '{}' with operator {} must have non-empty values",
                        self.key, self.operator,
                    ))));
                }
            }
            SelectorOperator::Exists | SelectorOperator::DoesNotExist => {
                if !self.values.is_empty() {
                    return Err(crate::ModelError::Invalid(Cow::Owned(format!(
                        "selector requirement '{}' with operator {} must have empty values",
                        self.key, self.operator,
                    ))));
                }
            }
        }
        Ok(())
    }

    /// Shorthand: require `key ∈ values`.
    #[inline]
    pub fn r#in(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::In,
            values,
        }
    }

    /// Shorthand: require `key ∉ values`.
    #[inline]
    pub fn not_in(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::NotIn,
            values,
        }
    }

    /// Shorthand: require label key to exist.
    #[inline]
    pub fn exists(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::Exists,
            values: vec![],
        }
    }

    /// Shorthand: require label key to NOT exist.
    #[inline]
    pub fn does_not_exist(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::DoesNotExist,
            values: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_constructor() {
        let req = SelectorRequirement::r#in("gpu", vec!["a100".into(), "h100".into()]);
        assert_eq!(req.key, "gpu");
        assert_eq!(req.operator, SelectorOperator::In);
        assert_eq!(req.values, vec!["a100", "h100"]);
    }

    #[test]
    fn not_in_constructor() {
        let req = SelectorRequirement::not_in("zone", vec!["us-west".into()]);
        assert_eq!(req.operator, SelectorOperator::NotIn);
    }

    #[test]
    fn exists_constructor() {
        let req = SelectorRequirement::exists("gpu");
        assert_eq!(req.operator, SelectorOperator::Exists);
        assert!(req.values.is_empty());
    }

    #[test]
    fn does_not_exist_constructor() {
        let req = SelectorRequirement::does_not_exist("tainted");
        assert_eq!(req.operator, SelectorOperator::DoesNotExist);
        assert!(req.values.is_empty());
    }

    #[test]
    fn serde_roundtrip() {
        let req = SelectorRequirement::r#in("tier", vec!["prod".into(), "staging".into()]);
        let json = serde_json::to_string(&req).unwrap();
        let back: SelectorRequirement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn serde_skips_empty_values() {
        let req = SelectorRequirement::exists("gpu");
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("values"),
            "empty values should be skipped: {json}"
        );
    }
}
