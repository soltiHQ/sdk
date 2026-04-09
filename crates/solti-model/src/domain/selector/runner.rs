//! # Runner selector.
//!
//! [`RunnerSelector`] matches runners by label equality and expression-based requirements.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{SelectorOperator, SelectorRequirement};
use crate::Labels;

/// Label selector for runner routing.
///
/// ```text
///  TaskSpec
///  ┌────────────────────────────────────────────────────────┐
///  │  runner_selector:                                      │
///  │    match_labels:      { "zone": "eu" }                 │
///  │    match_expressions: [ {key:"gpu", op:Exists} ]       │
///  └──────────────────────────┬─────────────────────────────┘
///                             │  ALL requirements ANDed
///                             ▼
///  RunnerRouter::pick()
///  ┌────────────────────────────────────────────────────────┐
///  │  Runner A  labels: {"zone":"us","gpu":"a100"}  ✗ skip  │
///  │  Runner B  labels: {"zone":"eu","gpu":"h100"}  ✓ match │
///  │  Runner C  labels: {"zone":"eu"}               ✗ skip  │
///  └────────────────────────────────────────────────────────┘
/// ```
///
/// Both `match_labels` and `match_expressions` are ANDed together.
/// An empty selector matches every runner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSelector {
    /// Exact key=value pairs: sugar for `In` with a single value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,

    /// Set-based requirements, ANDed with `match_labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<SelectorRequirement>,
}

impl RunnerSelector {
    /// Empty selector (matches everything).
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Selector from exact key=value pairs only.
    #[inline]
    pub fn from_labels(labels: BTreeMap<String, String>) -> Self {
        Self {
            match_labels: labels,
            match_expressions: vec![],
        }
    }

    /// Selector from expressions only.
    #[inline]
    pub fn from_expressions(expr: Vec<SelectorRequirement>) -> Self {
        Self {
            match_labels: BTreeMap::new(),
            match_expressions: expr,
        }
    }

    /// Returns `true` if the selector has no requirements (matches everything).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty() && self.match_expressions.is_empty()
    }

    /// Check whether `labels` satisfy **all** requirements of this selector.
    ///
    /// - Each `match_labels` entry requires an exact key=value match.
    /// - Each `match_expressions` entry is evaluated per its operator.
    /// - All requirements are ANDed.
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

    #[test]
    fn empty_selector_matches_everything() {
        let sel = RunnerSelector::new();
        assert!(sel.matches(&labels(&[])));
        assert!(sel.matches(&labels(&[("a", "b")])));
    }

    #[test]
    fn match_labels_exact_hit() {
        let sel = RunnerSelector::from_labels(BTreeMap::from([("zone".into(), "eu".into())]));
        assert!(sel.matches(&labels(&[("zone", "eu"), ("extra", "x")])));
    }

    #[test]
    fn match_labels_value_mismatch() {
        let sel = RunnerSelector::from_labels(BTreeMap::from([("zone".into(), "eu".into())]));
        assert!(!sel.matches(&labels(&[("zone", "us")])));
    }

    #[test]
    fn match_labels_key_missing() {
        let sel = RunnerSelector::from_labels(BTreeMap::from([("zone".into(), "eu".into())]));
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
            match_labels: BTreeMap::from([("zone".into(), "eu".into())]),
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
            match_labels: BTreeMap::from([("zone".into(), "eu".into())]),
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
        assert!(
            !RunnerSelector::from_labels(BTreeMap::from([("k".into(), "v".into()),])).is_empty()
        );
    }
}
