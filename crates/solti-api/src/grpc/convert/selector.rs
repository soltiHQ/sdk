//! # Selector Conversion
//!
//! Converts task labels and runner selectors in both directions.

use std::collections::HashMap;

use solti_model::{LabelSelector, Labels, SelectorOperator, SelectorRequirement};

use crate::{error::ApiError, proto_api};

pub(super) fn selector_to_proto(
    selector: &LabelSelector,
) -> Result<proto_api::LabelSelector, ApiError> {
    Ok(proto_api::LabelSelector {
        match_labels: selector
            .match_labels
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        match_expressions: selector
            .match_expressions
            .iter()
            .map(|requirement| {
                Ok(proto_api::SelectorRequirement {
                    key: requirement.key.clone(),
                    operator: operator_to_proto(requirement.operator)? as i32,
                    values: requirement.values.clone(),
                })
            })
            .collect::<Result<_, ApiError>>()?,
    })
}

pub(super) fn convert_label_selector(
    selector: proto_api::LabelSelector,
) -> Result<LabelSelector, ApiError> {
    let match_expressions = selector
        .match_expressions
        .into_iter()
        .map(|requirement| {
            let operator = match proto_api::SelectorOperator::try_from(requirement.operator) {
                Ok(proto_api::SelectorOperator::In) => SelectorOperator::In,
                Ok(proto_api::SelectorOperator::NotIn) => SelectorOperator::NotIn,
                Ok(proto_api::SelectorOperator::Exists) => SelectorOperator::Exists,
                Ok(proto_api::SelectorOperator::DoesNotExist) => SelectorOperator::DoesNotExist,
                _ => {
                    return Err(ApiError::InvalidRequest(format!(
                        "invalid selector operator for key '{}'",
                        requirement.key
                    )));
                }
            };
            Ok(SelectorRequirement {
                key: requirement.key,
                operator,
                values: requirement.values,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    let selector = LabelSelector {
        match_labels: convert_labels(selector.match_labels),
        match_expressions,
    };
    selector
        .validate()
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    Ok(selector)
}

pub(super) fn convert_labels(values: HashMap<String, String>) -> Labels {
    let mut labels = Labels::new();
    for (key, value) in values {
        labels.insert(key, value);
    }
    labels
}

fn operator_to_proto(operator: SelectorOperator) -> Result<proto_api::SelectorOperator, ApiError> {
    Ok(match operator {
        SelectorOperator::In => proto_api::SelectorOperator::In,
        SelectorOperator::NotIn => proto_api::SelectorOperator::NotIn,
        SelectorOperator::Exists => proto_api::SelectorOperator::Exists,
        SelectorOperator::DoesNotExist => proto_api::SelectorOperator::DoesNotExist,
        _ => {
            return Err(ApiError::Internal(
                "handler returned an unsupported selector operator".into(),
            ));
        }
    })
}
