//! Runner selector.
//!
//! [`RunnerSelector`] matches runners by label equality and expression-based requirements.

use serde::{Deserialize, Serialize};

use super::{SelectorOperator, SelectorRequirement};
use crate::Labels;

/// Label selector for runner routing.
///
/// Both `match_labels` and `match_expressions` are ANDed together.
/// An empty selector matches every runner.
///
/// ## Example
///
/// ```
/// use solti_model::{Labels, RunnerSelector, SelectorRequirement};
///
/// let selector = RunnerSelector {
///     match_labels: {
///         let mut labels = Labels::new();
///         labels.insert("zone", "eu");
///         labels
///     },
///     match_expressions: vec![SelectorRequirement::exists("gpu")],
/// };
///
/// let mut runner = Labels::new();
/// runner.insert("zone", "eu");
/// runner.insert("gpu", "h100");
///
/// assert!(selector.matches(&runner));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSelector {
    /// Exact key=value pairs: sugar for `In` with a single value.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub match_labels: Labels,

    /// Set-based requirements, ANDed with `match_labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<SelectorRequirement>,
}

impl RunnerSelector {
    /// Create an empty selector.
    ///
    /// An empty selector matches every runner.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, RunnerSelector};
    ///
    /// assert!(RunnerSelector::new().matches(&Labels::new()));
    /// ```
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Selector from exact key=value pairs only.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, RunnerSelector};
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("zone", "eu");
    ///
    /// let selector = RunnerSelector::from_labels(labels);
    ///
    /// let mut runner = Labels::new();
    /// runner.insert("zone", "eu");
    /// assert!(selector.matches(&runner));
    /// ```
    #[inline]
    pub fn from_labels(labels: Labels) -> Self {
        Self {
            match_labels: labels,
            match_expressions: vec![],
        }
    }

    /// Selector from expressions only.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, RunnerSelector, SelectorRequirement};
    ///
    /// let selector = RunnerSelector::from_expressions(vec![
    ///     SelectorRequirement::exists("gpu"),
    /// ]);
    ///
    /// let mut runner = Labels::new();
    /// runner.insert("gpu", "a100");
    /// assert!(selector.matches(&runner));
    /// ```
    #[inline]
    pub fn from_expressions(expr: Vec<SelectorRequirement>) -> Self {
        Self {
            match_labels: Labels::new(),
            match_expressions: expr,
        }
    }

    /// Return `true` if the selector has no requirements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty() && self.match_expressions.is_empty()
    }

    /// Check whether `labels` satisfy all requirements of this selector.
    ///
    /// - Each `match_labels` entry requires an exact key=value match.
    /// - Each `match_expressions` entry is evaluated per its operator.
    /// - All requirements are ANDed.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, RunnerSelector, SelectorRequirement};
    ///
    /// let selector = RunnerSelector::from_expressions(vec![
    ///     SelectorRequirement::r#in("gpu", vec!["a100".into(), "h100".into()]),
    /// ]);
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("gpu", "h100");
    ///
    /// assert!(selector.matches(&labels));
    /// ```
    pub fn matches(&self, labels: &Labels) -> bool {
        for (key, expected) in &self.match_labels {
            match labels.get(key) {
                Some(actual) if actual == expected => {}
                _ => return false,
            }
        }

        for req in &self.match_expressions {
            let value = labels.get(&req.key);
            let ok = match req.operator {
                SelectorOperator::In => match value {
                    Some(v) => req.values.iter().any(|x| x == v),
                    None => false,
                },
                SelectorOperator::NotIn => match value {
                    Some(v) => !req.values.iter().any(|x| x == v),
                    None => true,
                },
                SelectorOperator::Exists => value.is_some(),
                SelectorOperator::DoesNotExist => value.is_none(),
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        let mut l = Labels::new();
        for (k, v) in pairs {
            l.insert(*k, *v);
        }
        l
    }

    fn labels_of(pairs: &[(&str, &str)]) -> Labels {
        labels(pairs)
    }

    #[test]
    fn empty_selector_matches_everything() {
        let sel = RunnerSelector::new();
        assert!(sel.matches(&labels(&[])));
        assert!(sel.matches(&labels(&[("a", "b")])));
    }

    #[test]
    fn match_labels_exact_hit() {
        let sel = RunnerSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(sel.matches(&labels(&[("zone", "eu"), ("extra", "x")])));
    }

    #[test]
    fn match_labels_value_mismatch() {
        let sel = RunnerSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(!sel.matches(&labels(&[("zone", "us")])));
    }

    #[test]
    fn match_labels_key_missing() {
        let sel = RunnerSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(!sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_in_matches() {
        let sel = RunnerSelector::from_expressions(vec![SelectorRequirement::r#in(
            "gpu",
            vec!["a100".into(), "h100".into()],
        )]);
        assert!(sel.matches(&labels(&[("gpu", "a100")])));
        assert!(sel.matches(&labels(&[("gpu", "h100")])));
        assert!(!sel.matches(&labels(&[("gpu", "t4")])));
        assert!(!sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_not_in_matches() {
        let sel = RunnerSelector::from_expressions(vec![SelectorRequirement::not_in(
            "tier",
            vec!["dev".into()],
        )]);
        assert!(sel.matches(&labels(&[("tier", "prod")])));
        assert!(!sel.matches(&labels(&[("tier", "dev")])));
        assert!(sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_exists_matches() {
        let sel = RunnerSelector::from_expressions(vec![SelectorRequirement::exists("gpu")]);
        assert!(sel.matches(&labels(&[("gpu", "any")])));
        assert!(!sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_does_not_exist_matches() {
        let sel =
            RunnerSelector::from_expressions(vec![SelectorRequirement::does_not_exist("tainted")]);
        assert!(sel.matches(&labels(&[])));
        assert!(!sel.matches(&labels(&[("tainted", "true")])));
    }

    #[test]
    fn labels_and_expressions_anded() {
        let sel = RunnerSelector {
            match_labels: labels_of(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        assert!(sel.matches(&labels(&[("zone", "eu"), ("gpu", "a100")])));
        assert!(!sel.matches(&labels(&[("zone", "us"), ("gpu", "a100")])));
        assert!(!sel.matches(&labels(&[("zone", "eu")])));
    }

    #[test]
    fn multiple_expressions_anded() {
        let sel = RunnerSelector::from_expressions(vec![
            SelectorRequirement::r#in("tier", vec!["prod".into(), "staging".into()]),
            SelectorRequirement::does_not_exist("tainted"),
        ]);
        assert!(sel.matches(&labels(&[("tier", "prod")])));
        assert!(!sel.matches(&labels(&[("tier", "prod"), ("tainted", "true")])));
        assert!(!sel.matches(&labels(&[("tier", "dev")])));
    }

    #[test]
    fn serde_roundtrip() {
        let sel = RunnerSelector {
            match_labels: labels_of(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        let json = serde_json::to_string_pretty(&sel).unwrap();
        let back: RunnerSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }

    #[test]
    fn serde_empty_selector() {
        let sel = RunnerSelector::new();
        let json = serde_json::to_string(&sel).unwrap();
        assert_eq!(json, "{}");
        let back: RunnerSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }

    #[test]
    fn is_empty() {
        assert!(RunnerSelector::new().is_empty());
        assert!(!RunnerSelector::from_labels(labels_of(&[("k", "v")])).is_empty());
    }
}
