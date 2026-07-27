//! Kubernetes-style label selector.
//!
//! [`LabelSelector`] is shared by runner routing and resource queries.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use super::{SelectorOperator, SelectorRequirement};
use crate::{Labels, ModelError, ModelResult};

/// Label selector for matching any labeled object.
///
/// Both `match_labels` and `match_expressions` are ANDed together.
/// An empty selector matches every label set.
///
/// ## Example
///
/// ```
/// use solti_model::{Labels, LabelSelector, SelectorRequirement};
///
/// let selector = LabelSelector {
///     match_labels: {
///         let mut labels = Labels::new();
///         labels.insert("zone", "eu");
///         labels
///     },
///     match_expressions: vec![SelectorRequirement::exists("gpu")],
/// };
///
/// let mut labels = Labels::new();
/// labels.insert("zone", "eu");
/// labels.insert("gpu", "h100");
///
/// assert!(selector.matches(&labels));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelSelector {
    /// Exact key=value pairs: sugar for `In` with a single value.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub match_labels: Labels,

    /// Set-based requirements, ANDed with `match_labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<SelectorRequirement>,
}

impl LabelSelector {
    /// Create an empty selector.
    ///
    /// An empty selector matches every label set.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, LabelSelector};
    ///
    /// assert!(LabelSelector::new().matches(&Labels::new()));
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
    /// use solti_model::{Labels, LabelSelector};
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("zone", "eu");
    ///
    /// let selector = LabelSelector::from_labels(labels);
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("zone", "eu");
    /// assert!(selector.matches(&labels));
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
    /// use solti_model::{Labels, LabelSelector, SelectorRequirement};
    ///
    /// let selector = LabelSelector::from_expressions(vec![
    ///     SelectorRequirement::exists("gpu"),
    /// ]);
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("gpu", "a100");
    /// assert!(selector.matches(&labels));
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

    /// Validate exact matches and set-based requirements as a Kubernetes label selector.
    pub fn validate(&self) -> crate::ModelResult<()> {
        self.match_labels.validate()?;
        for requirement in &self.match_expressions {
            requirement.validate()?;
        }
        Ok(())
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
    /// use solti_model::{Labels, LabelSelector, SelectorRequirement};
    ///
    /// let selector = LabelSelector::from_expressions(vec![
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

impl FromStr for LabelSelector {
    type Err = ModelError;

    /// Parse Kubernetes label-selector syntax.
    ///
    /// Supported requirements are `=`, `==`, `!=`, `in`, `notin`, key
    /// existence and `!key` non-existence. Top-level commas mean AND.
    fn from_str(value: &str) -> ModelResult<Self> {
        let value = trim_selector_whitespace(value);
        if value.is_empty() {
            return Ok(Self::new());
        }

        let requirements = split_requirements(value)?
            .into_iter()
            .map(parse_requirement)
            .collect::<ModelResult<Vec<_>>>()?;
        let selector = Self::from_expressions(requirements);
        selector.validate()?;
        Ok(selector)
    }
}

impl fmt::Display for LabelSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut separator = |formatter: &mut fmt::Formatter<'_>| {
            if first {
                first = false;
                Ok(())
            } else {
                formatter.write_str(",")
            }
        };

        for (key, value) in &self.match_labels {
            separator(formatter)?;
            write!(formatter, "{key}={value}")?;
        }
        for requirement in &self.match_expressions {
            separator(formatter)?;
            match requirement.operator {
                SelectorOperator::In => write!(
                    formatter,
                    "{} in ({})",
                    requirement.key,
                    requirement.values.join(",")
                )?,
                SelectorOperator::NotIn => write!(
                    formatter,
                    "{} notin ({})",
                    requirement.key,
                    requirement.values.join(",")
                )?,
                SelectorOperator::Exists => formatter.write_str(&requirement.key)?,
                SelectorOperator::DoesNotExist => write!(formatter, "!{}", requirement.key)?,
            }
        }
        Ok(())
    }
}

fn split_requirements(value: &str) -> ModelResult<Vec<&str>> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0_u8;

    for (index, character) in value.char_indices() {
        match character {
            '(' => {
                if depth > 0 {
                    return Err(invalid_selector("nested parentheses are not allowed"));
                }
                depth = 1;
            }
            ')' => {
                if depth == 0 {
                    return Err(invalid_selector("unexpected closing parenthesis"));
                }
                depth = 0;
            }
            ',' if depth == 0 => {
                let requirement = trim_selector_whitespace(&value[start..index]);
                if requirement.is_empty() {
                    return Err(invalid_selector("empty requirement"));
                }
                result.push(requirement);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }

    if depth != 0 {
        return Err(invalid_selector("unclosed parenthesis"));
    }
    let requirement = trim_selector_whitespace(&value[start..]);
    if requirement.is_empty() {
        return Err(invalid_selector("empty requirement"));
    }
    result.push(requirement);
    Ok(result)
}

fn parse_requirement(value: &str) -> ModelResult<SelectorRequirement> {
    if let Some(key) = value.strip_prefix('!') {
        let key = trim_selector_whitespace(key);
        if key.is_empty() {
            return Err(invalid_selector("missing key after `!`"));
        }
        return Ok(SelectorRequirement::does_not_exist(key));
    }

    if let Some(open) = value.find('(') {
        let close = value
            .rfind(')')
            .ok_or_else(|| invalid_selector("unclosed parenthesis"))?;
        if !trim_selector_whitespace(&value[close + 1..]).is_empty() {
            return Err(invalid_selector(
                "unexpected text after closing parenthesis",
            ));
        }

        let head = trim_selector_whitespace_end(&value[..open]);
        let (key, operator) = if let Some(key) = head.strip_suffix("notin")
            && key
                .as_bytes()
                .last()
                .is_some_and(|byte| is_selector_whitespace(*byte))
        {
            (trim_selector_whitespace_end(key), SelectorOperator::NotIn)
        } else if let Some(key) = head.strip_suffix("in")
            && key
                .as_bytes()
                .last()
                .is_some_and(|byte| is_selector_whitespace(*byte))
        {
            (trim_selector_whitespace_end(key), SelectorOperator::In)
        } else {
            return Err(invalid_selector("expected `in` or `notin` before `(`"));
        };
        if key.is_empty() {
            return Err(invalid_selector("missing key before set operator"));
        }

        let values = trim_selector_whitespace(&value[open + 1..close]);
        let values: Vec<_> = values
            .split(',')
            .map(trim_selector_whitespace)
            .map(|value| value.to_owned())
            .collect();
        return Ok(SelectorRequirement {
            key: key.to_owned(),
            operator,
            values,
        });
    }

    for (token, operator) in [
        ("!=", SelectorOperator::NotIn),
        ("==", SelectorOperator::In),
        ("=", SelectorOperator::In),
    ] {
        if let Some((key, selected)) = value.split_once(token) {
            let key = trim_selector_whitespace(key);
            if key.is_empty() {
                return Err(invalid_selector("missing key before equality operator"));
            }
            return Ok(SelectorRequirement {
                key: key.to_owned(),
                operator,
                values: vec![trim_selector_whitespace(selected).to_owned()],
            });
        }
    }

    Ok(SelectorRequirement::exists(trim_selector_whitespace(value)))
}

fn trim_selector_whitespace(value: &str) -> &str {
    trim_selector_whitespace_end(value.trim_start_matches(|character: char| {
        character.is_ascii() && is_selector_whitespace(character as u8)
    }))
}

fn trim_selector_whitespace_end(value: &str) -> &str {
    value.trim_end_matches(|character: char| {
        character.is_ascii() && is_selector_whitespace(character as u8)
    })
}

const fn is_selector_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn invalid_selector(message: &str) -> ModelError {
    ModelError::Invalid(format!("invalid label selector: {message}").into())
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
        let sel = LabelSelector::new();
        assert!(sel.matches(&labels(&[])));
        assert!(sel.matches(&labels(&[("a", "b")])));
    }

    #[test]
    fn match_labels_exact_hit() {
        let sel = LabelSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(sel.matches(&labels(&[("zone", "eu"), ("extra", "x")])));
    }

    #[test]
    fn match_labels_value_mismatch() {
        let sel = LabelSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(!sel.matches(&labels(&[("zone", "us")])));
    }

    #[test]
    fn match_labels_key_missing() {
        let sel = LabelSelector::from_labels(labels_of(&[("zone", "eu")]));
        assert!(!sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_in_matches() {
        let sel = LabelSelector::from_expressions(vec![SelectorRequirement::r#in(
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
        let sel = LabelSelector::from_expressions(vec![SelectorRequirement::not_in(
            "tier",
            vec!["dev".into()],
        )]);
        assert!(sel.matches(&labels(&[("tier", "prod")])));
        assert!(!sel.matches(&labels(&[("tier", "dev")])));
        assert!(sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_exists_matches() {
        let sel = LabelSelector::from_expressions(vec![SelectorRequirement::exists("gpu")]);
        assert!(sel.matches(&labels(&[("gpu", "any")])));
        assert!(!sel.matches(&labels(&[])));
    }

    #[test]
    fn expr_does_not_exist_matches() {
        let sel =
            LabelSelector::from_expressions(vec![SelectorRequirement::does_not_exist("tainted")]);
        assert!(sel.matches(&labels(&[])));
        assert!(!sel.matches(&labels(&[("tainted", "true")])));
    }

    #[test]
    fn labels_and_expressions_anded() {
        let sel = LabelSelector {
            match_labels: labels_of(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        assert!(sel.matches(&labels(&[("zone", "eu"), ("gpu", "a100")])));
        assert!(!sel.matches(&labels(&[("zone", "us"), ("gpu", "a100")])));
        assert!(!sel.matches(&labels(&[("zone", "eu")])));
    }

    #[test]
    fn multiple_expressions_anded() {
        let sel = LabelSelector::from_expressions(vec![
            SelectorRequirement::r#in("tier", vec!["prod".into(), "staging".into()]),
            SelectorRequirement::does_not_exist("tainted"),
        ]);
        assert!(sel.matches(&labels(&[("tier", "prod")])));
        assert!(!sel.matches(&labels(&[("tier", "prod"), ("tainted", "true")])));
        assert!(!sel.matches(&labels(&[("tier", "dev")])));
    }

    #[test]
    fn serde_roundtrip() {
        let sel = LabelSelector {
            match_labels: labels_of(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        let json = serde_json::to_string_pretty(&sel).unwrap();
        let back: LabelSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }

    #[test]
    fn serde_empty_selector() {
        let sel = LabelSelector::new();
        let json = serde_json::to_string(&sel).unwrap();
        assert_eq!(json, "{}");
        let back: LabelSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }

    #[test]
    fn is_empty() {
        assert!(LabelSelector::new().is_empty());
        assert!(!LabelSelector::from_labels(labels_of(&[("k", "v")])).is_empty());
    }

    #[test]
    fn validate_checks_match_labels_and_expressions() {
        let mut invalid = Labels::new();
        invalid.insert("bad key", "value");
        assert!(LabelSelector::from_labels(invalid).validate().is_err());

        let selector = LabelSelector::from_expressions(vec![SelectorRequirement::exists(
            "example.io/capability",
        )]);
        selector.validate().unwrap();
    }

    #[test]
    fn parses_kubernetes_selector_syntax() {
        let selector: LabelSelector =
            "environment=production,tier in (frontend,backend),track!=canary,!tainted,gpu"
                .parse()
                .unwrap();

        assert!(selector.matches(&labels(&[
            ("environment", "production"),
            ("tier", "frontend"),
            ("track", "stable"),
            ("gpu", "h100"),
        ])));
        assert!(!selector.matches(&labels(&[
            ("environment", "production"),
            ("tier", "worker"),
            ("track", "stable"),
            ("gpu", "h100"),
        ])));
    }

    #[test]
    fn parses_double_equality_and_empty_selector() {
        let selector: LabelSelector = "release==stable".parse().unwrap();
        assert!(selector.matches(&labels(&[("release", "stable")])));
        assert!("".parse::<LabelSelector>().unwrap().is_empty());
    }

    #[test]
    fn parses_empty_kubernetes_label_values() {
        let selector: LabelSelector = "x in (foo,,baz),z notin ()".parse().unwrap();

        assert!(selector.matches(&labels(&[("x", ""), ("z", "value")])));
        assert!(!selector.matches(&labels(&[("x", "foo"), ("z", "")])));
        assert!(
            "key="
                .parse::<LabelSelector>()
                .unwrap()
                .matches(&labels(&[("key", "")]))
        );
    }

    #[test]
    fn display_round_trips_empty_label_values() {
        for value in ["key=", "key in ()", "key in (foo,,baz)", "key notin ()"] {
            let selector: LabelSelector = value.parse().unwrap();
            let reparsed: LabelSelector = selector.to_string().parse().unwrap();
            assert_eq!(reparsed, selector, "selector must round-trip: {value}");
        }
    }

    #[test]
    fn parser_uses_kubernetes_ascii_whitespace() {
        for value in [
            "\u{00a0}tier in (frontend)",
            "tier\u{00a0}in (frontend)",
            "tier in (\u{00a0}frontend)",
        ] {
            assert!(
                value.parse::<LabelSelector>().is_err(),
                "selector must be rejected: {value:?}"
            );
        }

        " \t\r\ntier\tin\n(frontend)\r\n"
            .parse::<LabelSelector>()
            .unwrap();
    }

    #[test]
    fn negative_requirements_match_a_missing_key() {
        assert!(
            "tier!=frontend"
                .parse::<LabelSelector>()
                .unwrap()
                .matches(&Labels::new())
        );
        assert!(
            "tier notin (frontend)"
                .parse::<LabelSelector>()
                .unwrap()
                .matches(&Labels::new())
        );
    }

    #[test]
    fn display_uses_kubernetes_selector_syntax() {
        let selector = LabelSelector {
            match_labels: labels_of(&[("environment", "production")]),
            match_expressions: vec![
                SelectorRequirement::r#in("tier", vec!["frontend".into(), "backend".into()]),
                SelectorRequirement::does_not_exist("tainted"),
            ],
        };
        assert_eq!(
            selector.to_string(),
            "environment=production,tier in (frontend,backend),!tainted"
        );
    }

    #[test]
    fn malformed_selector_is_rejected() {
        for value in [
            ",environment=production",
            "environment=production,",
            "tier in (frontend",
            "tier around (frontend)",
            "!",
            "bad key=value",
            "tier in (front@end)",
        ] {
            assert!(
                value.parse::<LabelSelector>().is_err(),
                "selector must be rejected: {value}"
            );
        }
    }
}
