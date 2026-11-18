//! JSON Schema helpers for the built-in workload bodies.

use schemars::{Schema, json_schema};

/// A string with at least one non-whitespace character.
pub(crate) fn non_empty_string(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "pattern": "\\S"
    })
}

/// A padded base64 script body, bounded by the largest accepted encoded length.
#[cfg(feature = "subprocess")]
pub(crate) fn script_body(_generator: &mut schemars::SchemaGenerator) -> Schema {
    let max_encoded_len = crate::subprocess::MAX_SCRIPT_BODY_BYTES.div_ceil(3) * 4;
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": max_encoded_len,
        "contentEncoding": "base64"
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use serde_json::Value;

    /// Collects every integer subschema of `schema` that does not bound both sides to the
    /// range a JSON number carries exactly (Review Focus 6).
    pub(crate) fn unbounded_integers(schema: &Value) -> Vec<String> {
        fn within(value: Option<&Value>) -> bool {
            value
                .and_then(Value::as_i64)
                .is_some_and(|bound| bound.abs() <= solti_model::MAX_JSON_SAFE_INTEGER)
        }

        fn walk(value: &Value, path: &str, found: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    let integer = match map.get("type") {
                        Some(Value::String(name)) => name == "integer",
                        Some(Value::Array(names)) => names.iter().any(|name| name == "integer"),
                        _ => false,
                    };
                    if integer && !(within(map.get("minimum")) && within(map.get("maximum"))) {
                        found.push(path.to_owned());
                    }
                    for (key, child) in map {
                        walk(child, &format!("{path}/{key}"), found);
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(child, &format!("{path}/{index}"), found);
                    }
                }
                _ => {}
            }
        }

        let mut found = Vec::new();
        walk(schema, "", &mut found);
        found
    }

    #[test]
    fn the_scan_flags_an_unbounded_integer_and_accepts_a_bounded_one() {
        let unbounded = serde_json::json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "format": "uint64", "minimum": 0}}
        });
        assert_eq!(unbounded_integers(&unbounded), vec!["/properties/n"]);

        let bounded = serde_json::json!({
            "type": "object",
            "properties": {"n": {
                "type": "integer", "minimum": 0, "maximum": 9_007_199_254_740_991_i64
            }}
        });
        assert!(unbounded_integers(&bounded).is_empty());

        let past = serde_json::json!({"type": "integer", "minimum": 0, "maximum": 9_007_199_254_740_992_i64});
        assert_eq!(unbounded_integers(&past), vec![""]);
    }
}
