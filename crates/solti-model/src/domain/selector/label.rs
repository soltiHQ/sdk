//! # Label selector
//!
//! [`LabelSelector`] follows Kubernetes label selector syntax and matching rules.
//! Struct construction and direct deserialization do not validate requirements.
//! Call [`LabelSelector::validate`] at an input boundary.

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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelSelector {
    /// Exact key-value matches.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub match_labels: Labels,

    /// Set-based requirements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<SelectorRequirement>,
}

impl LabelSelector {
    /// Creates an empty selector.
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

    /// Creates a selector from exact matches.
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

    /// Creates a selector from expressions.
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

    /// Returns whether the selector is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.match_labels.is_empty() && self.match_expressions.is_empty()
    }

    /// Validates the selector.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when a key, value, or requirement is invalid.
    pub fn validate(&self) -> crate::ModelResult<()> {
        self.match_labels.validate()?;
        for requirement in &self.match_expressions {
            requirement.validate()?;
        }
        Ok(())
    }

    /// Returns whether labels satisfy every requirement.
    ///
    /// `match_labels` and `match_expressions` are ANDed.
    /// `NotIn` matches when the key is absent.
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

    /// Parses Kubernetes label selector syntax.
    ///
    /// Supported requirements are `=`, `==`, `!=`, `in`, `notin`, key existence and `!key` non-existence. Top-level commas mean AND.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the syntax or a requirement is invalid.
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
        let mut labels = Labels::new();
        for (key, value) in pairs {
            labels.insert(*key, *value);
        }
        labels
    }

    #[test]
    fn empty_and_exact_label_matching() {
        let empty = LabelSelector::new();
        assert!(empty.is_empty());
        assert!(empty.matches(&labels(&[])));
        assert!(empty.matches(&labels(&[("a", "b")])));

        let selector = LabelSelector::from_labels(labels(&[("zone", "eu")]));
        assert!(!selector.is_empty());
        assert!(selector.matches(&labels(&[("zone", "eu"), ("extra", "x")])));
        assert!(!selector.matches(&labels(&[("zone", "us")])));
        assert!(!selector.matches(&labels(&[])));
    }

    #[test]
    fn set_operators_follow_kubernetes_missing_key_semantics() {
        let included = LabelSelector::from_expressions(vec![SelectorRequirement::r#in(
            "gpu",
            vec!["a100".into(), "h100".into()],
        )]);
        assert!(included.matches(&labels(&[("gpu", "a100")])));
        assert!(included.matches(&labels(&[("gpu", "h100")])));
        assert!(!included.matches(&labels(&[("gpu", "t4")])));
        assert!(!included.matches(&labels(&[])));

        let excluded = LabelSelector::from_expressions(vec![SelectorRequirement::not_in(
            "tier",
            vec!["dev".into()],
        )]);
        assert!(excluded.matches(&labels(&[("tier", "prod")])));
        assert!(!excluded.matches(&labels(&[("tier", "dev")])));
        assert!(excluded.matches(&labels(&[])));

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
    fn existence_operators_match_presence() {
        let exists = LabelSelector::from_expressions(vec![SelectorRequirement::exists("gpu")]);
        assert!(exists.matches(&labels(&[("gpu", "any")])));
        assert!(!exists.matches(&labels(&[])));

        let missing =
            LabelSelector::from_expressions(vec![SelectorRequirement::does_not_exist("tainted")]);
        assert!(missing.matches(&labels(&[])));
        assert!(!missing.matches(&labels(&[("tainted", "true")])));
    }

    #[test]
    fn labels_and_expressions_are_anded() {
        let selector = LabelSelector {
            match_labels: labels(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        assert!(selector.matches(&labels(&[("zone", "eu"), ("gpu", "a100")])));
        assert!(!selector.matches(&labels(&[("zone", "us"), ("gpu", "a100")])));
        assert!(!selector.matches(&labels(&[("zone", "eu")])));

        let expressions = LabelSelector::from_expressions(vec![
            SelectorRequirement::r#in("tier", vec!["prod".into(), "staging".into()]),
            SelectorRequirement::does_not_exist("tainted"),
        ]);
        assert!(expressions.matches(&labels(&[("tier", "prod")])));
        assert!(!expressions.matches(&labels(&[("tier", "prod"), ("tainted", "true")])));
        assert!(!expressions.matches(&labels(&[("tier", "dev")])));
    }

    #[test]
    fn serde_roundtrip_and_empty_shape_are_stable() {
        let selector = LabelSelector {
            match_labels: labels(&[("zone", "eu")]),
            match_expressions: vec![SelectorRequirement::exists("gpu")],
        };
        let json = serde_json::to_string_pretty(&selector).unwrap();
        let back: LabelSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, selector);

        let empty = LabelSelector::new();
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, "{}");
        assert_eq!(serde_json::from_str::<LabelSelector>(&json).unwrap(), empty);
    }

    #[test]
    fn validation_checks_labels_and_expressions() {
        let mut invalid = Labels::new();
        invalid.insert("bad key", "value");
        assert!(LabelSelector::from_labels(invalid).validate().is_err());

        let selector = LabelSelector::from_expressions(vec![SelectorRequirement::exists(
            "example.io/capability",
        )]);
        selector.validate().unwrap();
    }

    #[test]
    fn parser_and_display_use_kubernetes_selector_syntax() {
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

        let selector: LabelSelector = "release==stable".parse().unwrap();
        assert!(selector.matches(&labels(&[("release", "stable")])));
        assert!("".parse::<LabelSelector>().unwrap().is_empty());

        let rendered = LabelSelector {
            match_labels: labels(&[("environment", "production")]),
            match_expressions: vec![
                SelectorRequirement::r#in("tier", vec!["frontend".into(), "backend".into()]),
                SelectorRequirement::does_not_exist("tainted"),
            ],
        };
        assert_eq!(
            rendered.to_string(),
            "environment=production,tier in (frontend,backend),!tainted"
        );
    }

    #[test]
    fn empty_values_roundtrip_and_match_kubernetes_semantics() {
        let selector: LabelSelector = "x in (foo,,baz),z notin ()".parse().unwrap();
        assert!(selector.matches(&labels(&[("x", ""), ("z", "value")])));
        assert!(!selector.matches(&labels(&[("x", "foo"), ("z", "")])));
        assert!(
            "key="
                .parse::<LabelSelector>()
                .unwrap()
                .matches(&labels(&[("key", "")]))
        );

        for value in ["key=", "key in ()", "key in (foo,,baz)", "key notin ()"] {
            let selector: LabelSelector = value.parse().unwrap();
            let reparsed: LabelSelector = selector.to_string().parse().unwrap();
            assert_eq!(reparsed, selector, "selector must round-trip: {value}");
        }
    }

    #[test]
    fn parser_accepts_ascii_whitespace_and_rejects_malformed_input() {
        " \t\r\ntier\tin\n(frontend)\r\n"
            .parse::<LabelSelector>()
            .unwrap();

        for value in [
            "\u{00a0}tier in (frontend)",
            "tier\u{00a0}in (frontend)",
            "tier in (\u{00a0}frontend)",
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
                "selector must be rejected: {value:?}"
            );
        }
    }
}
