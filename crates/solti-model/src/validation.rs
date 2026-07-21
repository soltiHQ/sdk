use crate::{ModelError, ModelResult};

pub(crate) const DNS1123_SUBDOMAIN_MAX_LEN: usize = 253;
const DNS1035_LABEL_MAX_LEN: usize = 63;
const QUALIFIED_NAME_MAX_LEN: usize = 63;

pub(crate) fn validate_dns1123_subdomain(field: &str, value: &str) -> ModelResult<()> {
    if value.is_empty() {
        return invalid(format!("{field} must not be empty"));
    }
    if value.len() > DNS1123_SUBDOMAIN_MAX_LEN {
        return invalid(format!(
            "{field} length {} exceeds max {DNS1123_SUBDOMAIN_MAX_LEN}",
            value.len()
        ));
    }
    if !value.split('.').all(is_dns1123_segment) {
        return invalid(format!("{field} must be a Kubernetes DNS-1123 subdomain"));
    }
    Ok(())
}

pub(crate) fn validate_dns1035_label(field: &str, value: &str) -> ModelResult<()> {
    if value.is_empty() {
        return invalid(format!("{field} must not be empty"));
    }
    if value.len() > DNS1035_LABEL_MAX_LEN {
        return invalid(format!(
            "{field} length {} exceeds max {DNS1035_LABEL_MAX_LEN}",
            value.len()
        ));
    }

    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase()
        || !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return invalid(format!("{field} must be a Kubernetes DNS-1035 label"));
    }
    Ok(())
}

pub(crate) fn validate_crd_api_version<'a>(
    field: &str,
    api_version: &'a str,
) -> ModelResult<&'a str> {
    let mut parts = api_version.split('/');
    let group = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || group.is_empty() || version.is_empty() {
        return invalid(format!(
            "{field} must have the Kubernetes CRD form `<group>/<version>`"
        ));
    }
    validate_dns1123_subdomain(&format!("{field} group"), group)?;
    if !group.contains('.') {
        return invalid(format!("{field} group must contain at least one dot"));
    }
    validate_dns1035_label(&format!("{field} version"), version)?;
    Ok(group)
}

pub(crate) fn validate_crd_kind(field: &str, kind: &str) -> ModelResult<()> {
    validate_dns1035_label(field, &kind.to_ascii_lowercase())
}

pub(crate) fn validate_qualified_name(field: &str, value: &str) -> ModelResult<()> {
    let mut parts = value.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        return invalid(format!(
            "{field} must contain at most one DNS prefix separator `/`"
        ));
    }

    let name = if let Some(name) = second {
        validate_dns1123_subdomain(&format!("{field} prefix"), first)?;
        name
    } else {
        first
    };
    validate_qualified_name_part(field, name, false)
}

pub(crate) fn validate_label_value(field: &str, value: &str) -> ModelResult<()> {
    validate_qualified_name_part(field, value, true)
}

fn validate_qualified_name_part(field: &str, value: &str, allow_empty: bool) -> ModelResult<()> {
    if value.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            invalid(format!("{field} name must not be empty"))
        };
    }
    if value.len() > QUALIFIED_NAME_MAX_LEN {
        return invalid(format!(
            "{field} name length {} exceeds max {QUALIFIED_NAME_MAX_LEN}",
            value.len()
        ));
    }

    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.'))
    {
        return invalid(format!(
            "{field} name must follow Kubernetes qualified-name rules"
        ));
    }
    Ok(())
}

fn is_dns1123_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && (bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn invalid<T>(message: String) -> ModelResult<T> {
    Err(ModelError::Invalid(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns1123_subdomain_matches_kubernetes_shape_and_total_limit() {
        validate_dns1123_subdomain("name", "a".repeat(253).as_str()).unwrap();
        validate_dns1123_subdomain("name", "api.worker-1.example").unwrap();

        for invalid in ["", "Upper", "under_score", "-start", "end-", "a..b"] {
            assert!(validate_dns1123_subdomain("name", invalid).is_err());
        }
        assert!(validate_dns1123_subdomain("name", &"a".repeat(254)).is_err());
    }

    #[test]
    fn crd_api_version_requires_group_and_dns1035_version() {
        assert_eq!(
            validate_crd_api_version("apiVersion", "workloads.example.io/v1alpha1").unwrap(),
            "workloads.example.io"
        );

        for invalid in [
            "",
            "v1",
            "example/v1",
            "Example.io/v1",
            "example.io/1v",
            "example.io/v1/extra",
        ] {
            assert!(validate_crd_api_version("apiVersion", invalid).is_err());
        }
    }

    #[test]
    fn crd_kind_uses_lowercased_dns1035_validation() {
        for valid in ["Task", "ImageResize", "task-v2"] {
            validate_crd_kind("kind", valid).unwrap();
        }
        for invalid in ["", "1Task", "Bad Kind", "_Task", "Task_"] {
            assert!(validate_crd_kind("kind", invalid).is_err());
        }
    }

    #[test]
    fn qualified_names_and_label_values_match_kubernetes_rules() {
        for valid in ["name", "app.kubernetes.io/name", "UPPER_name.1"] {
            validate_qualified_name("key", valid).unwrap();
        }
        for invalid in ["", "/name", "Example.io/name", "example.io/", "a/b/c"] {
            assert!(validate_qualified_name("key", invalid).is_err());
        }
        validate_label_value("value", "").unwrap();
        validate_label_value("value", "UPPER_name.1").unwrap();
        assert!(validate_label_value("value", "-bad").is_err());
    }
}
