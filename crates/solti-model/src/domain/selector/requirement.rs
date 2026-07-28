//! # Selector requirement
//!
//! [`SelectorRequirement`] is one constraint inside a [`LabelSelector`](crate::LabelSelector).
//! Constructors set fields but do not validate them.

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
    /// Values used by `In` and `NotIn`.
    ///
    /// This must be empty for `Exists` and `DoesNotExist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

impl SelectorRequirement {
    /// Validates the requirement.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ModelError::Invalid`].
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

    /// Creates an `In` requirement.
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

    /// Creates a `NotIn` requirement.
    #[inline]
    pub fn not_in(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::NotIn,
            values,
        }
    }

    /// Creates an `Exists` requirement.
    #[inline]
    pub fn exists(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            operator: SelectorOperator::Exists,
            values: vec![],
        }
    }

    /// Creates a `DoesNotExist` requirement.
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
    fn constructors_set_operator_key_and_values() {
        let included = SelectorRequirement::r#in("gpu", vec!["a100".into(), "h100".into()]);
        assert_eq!(included.key, "gpu");
        assert_eq!(included.operator, SelectorOperator::In);
        assert_eq!(included.values, vec!["a100", "h100"]);

        let excluded = SelectorRequirement::not_in("zone", vec!["us-west".into()]);
        assert_eq!(excluded.key, "zone");
        assert_eq!(excluded.operator, SelectorOperator::NotIn);
        assert_eq!(excluded.values, vec!["us-west"]);

        for (requirement, operator) in [
            (SelectorRequirement::exists("gpu"), SelectorOperator::Exists),
            (
                SelectorRequirement::does_not_exist("tainted"),
                SelectorOperator::DoesNotExist,
            ),
        ] {
            assert_eq!(requirement.operator, operator);
            assert!(requirement.values.is_empty());
        }
    }

    #[test]
    fn serde_roundtrip_and_empty_values_shape_are_stable() {
        let req = SelectorRequirement::r#in("tier", vec!["prod".into(), "staging".into()]);
        let json = serde_json::to_string(&req).unwrap();
        let back: SelectorRequirement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);

        let req = SelectorRequirement::exists("gpu");
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("values"),
            "empty values should be skipped: {json}"
        );
    }

    #[test]
    fn validation_uses_kubernetes_label_rules() {
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
