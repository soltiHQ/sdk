//! Selector requirement.
//!
//! [`SelectorRequirement`] is a single label constraint used in [`LabelSelector`](crate::LabelSelector).

use serde::{Deserialize, Serialize};

use super::SelectorOperator;

/// Single set-based requirement for label matching.
///
/// Used inside [`super::LabelSelector::match_expressions`].
///
/// ## Example
///
/// ```
/// use solti_model::{SelectorOperator, SelectorRequirement};
///
/// let req = SelectorRequirement::r#in("gpu", vec!["a100".into(), "h100".into()]);
///
/// assert_eq!(req.key, "gpu");
/// assert_eq!(req.operator, SelectorOperator::In);
/// req.validate().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::SelectorRequirement;
    ///
    /// assert!(SelectorRequirement::exists("gpu").validate().is_ok());
    /// assert!(SelectorRequirement::r#in("gpu", vec![]).validate().is_err());
    /// ```
    pub fn validate(&self) -> crate::error::ModelResult<()> {
        use std::borrow::Cow;

        crate::validation::validate_qualified_name("selector requirement key", &self.key)?;
        match self.operator {
            SelectorOperator::In | SelectorOperator::NotIn => {
                if self.values.is_empty() {
                    return Err(crate::ModelError::Invalid(Cow::Owned(format!(
                        "selector requirement '{}' with operator {} must have non-empty values",
                        self.key, self.operator,
                    ))));
                }
                for value in &self.values {
                    crate::validation::validate_label_value("selector requirement value", value)?;
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

    /// Shorthand: require `key` to be in `values`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{SelectorOperator, SelectorRequirement};
    ///
    /// let req = SelectorRequirement::r#in("gpu", vec!["h100".into()]);
    /// assert_eq!(req.operator, SelectorOperator::In);
    /// ```
    #[inline]
    pub fn r#in(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::In,
            values,
        }
    }

    /// Shorthand: require `key` to not be in `values`.
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

    /// Shorthand: require label key to not exist.
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

    #[test]
    fn validate_uses_kubernetes_label_rules() {
        SelectorRequirement::r#in(
            "workloads.example.io/class",
            vec!["gpu_fast".into(), "".into()],
        )
        .validate()
        .unwrap();

        assert!(SelectorRequirement::exists("bad key").validate().is_err());
        assert!(
            SelectorRequirement::r#in("valid", vec!["-invalid".into()])
                .validate()
                .is_err()
        );
    }
}
