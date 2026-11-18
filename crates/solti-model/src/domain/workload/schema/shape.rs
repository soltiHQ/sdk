//! # Schema shape
//!
//! The bounds Podium checks on the canonical bytes of a schema before it
//! decodes them (`canonical` and `checkShape` in
//! `internal/workloadschema/schema.go`).

use super::{
    MAX_WORKLOAD_SCHEMA_BYTES, MAX_WORKLOAD_SCHEMA_DEPTH, MAX_WORKLOAD_SCHEMA_VALUES,
    WorkloadSchemaError, WorkloadSchemaErrorReason,
};

/// Refuses canonical bytes over [`MAX_WORKLOAD_SCHEMA_BYTES`].
pub(super) fn check_size(canonical: &[u8]) -> Result<(), WorkloadSchemaError> {
    if canonical.len() > MAX_WORKLOAD_SCHEMA_BYTES {
        return Err(WorkloadSchemaError::root(
            WorkloadSchemaErrorReason::TooLarge {
                bytes: canonical.len(),
                limit: MAX_WORKLOAD_SCHEMA_BYTES,
            },
        ));
    }
    Ok(())
}

/// Refuses canonical bytes whose objects and arrays nest deeper than
/// [`MAX_WORKLOAD_SCHEMA_DEPTH`], or that hold more than
/// [`MAX_WORKLOAD_SCHEMA_VALUES`] objects and booleans.
///
/// Outside its strings, canonical JSON has a `t` or an `f` only where a `true`
/// or a `false` begins, so the scan counts exactly what Podium counts.
pub(super) fn check_shape(canonical: &[u8]) -> Result<(), WorkloadSchemaError> {
    let mut depth = 0_usize;
    let mut values = 0_usize;
    let mut in_string = false;
    let mut bytes = canonical.iter();
    while let Some(&byte) = bytes.next() {
        if in_string {
            match byte {
                // The escaped byte, which may be a quote.
                b'\\' => {
                    bytes.next();
                }
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                if byte == b'{' {
                    values += 1;
                }
                depth += 1;
                if depth > MAX_WORKLOAD_SCHEMA_DEPTH {
                    return Err(WorkloadSchemaError::root(
                        WorkloadSchemaErrorReason::TooDeep {
                            limit: MAX_WORKLOAD_SCHEMA_DEPTH,
                        },
                    ));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b't' | b'f' => values += 1,
            _ => {}
        }
    }
    if values > MAX_WORKLOAD_SCHEMA_VALUES {
        return Err(WorkloadSchemaError::root(
            WorkloadSchemaErrorReason::TooManyValues {
                count: values,
                limit: MAX_WORKLOAD_SCHEMA_VALUES,
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use crate::{
        MAX_WORKLOAD_SCHEMA_BYTES, MAX_WORKLOAD_SCHEMA_DEPTH, MAX_WORKLOAD_SCHEMA_VALUES,
        WorkloadSchema, WorkloadSchemaErrorReason as Reason,
    };

    /// `{"description":"xx…x"}` of exactly `bytes` canonical bytes.
    fn described(bytes: usize) -> Value {
        let overhead = r#"{"description":""}"#.len();
        json!({"description": "x".repeat(bytes - overhead)})
    }

    /// A schema whose objects and arrays nest `depth` levels: the root object
    /// is level 1, and each array of the `const` adds one.
    fn nested_const(depth: usize) -> Value {
        let mut value = json!([]);
        for _ in 2..depth {
            value = json!([value]);
        }
        json!({"const": value})
    }

    #[test]
    fn schema_size_is_bounded_on_canonical_bytes() {
        assert_eq!(MAX_WORKLOAD_SCHEMA_BYTES, 65_536);

        let at_limit = WorkloadSchema::from_value(&described(65_536)).unwrap();
        assert_eq!(at_limit.canonical_json().len(), 65_536);

        let err = WorkloadSchema::from_value(&described(65_537)).unwrap_err();
        assert_eq!(err.location, "#");
        assert_eq!(
            err.reason,
            Reason::TooLarge {
                bytes: 65_537,
                limit: 65_536
            }
        );
    }

    #[test]
    fn schema_depth_is_bounded_at_32() {
        assert_eq!(MAX_WORKLOAD_SCHEMA_DEPTH, 32);

        WorkloadSchema::from_value(&nested_const(32)).unwrap();

        let err = WorkloadSchema::from_value(&nested_const(33)).unwrap_err();
        assert_eq!(err.location, "#");
        assert_eq!(err.reason, Reason::TooDeep { limit: 32 });
    }

    #[test]
    fn schema_values_are_bounded_at_512_objects_and_booleans() {
        assert_eq!(MAX_WORKLOAD_SCHEMA_VALUES, 512);

        // The root object is one value; the booleans of an `enum` count too.
        let booleans = |n: usize| json!({"enum": vec![true; n]});
        WorkloadSchema::from_value(&booleans(511)).unwrap();
        let err = WorkloadSchema::from_value(&booleans(512)).unwrap_err();
        assert_eq!(err.location, "#");
        assert_eq!(
            err.reason,
            Reason::TooManyValues {
                count: 513,
                limit: 512
            }
        );

        // Objects count wherever they are; arrays, strings, numbers and null
        // do not. The root and the `properties` object are two of them.
        let objects = |n: usize| {
            let properties: Map<String, Value> =
                (0..n).map(|i| (format!("p{i}"), json!({}))).collect();
            json!({"properties": properties, "enum": [null, 1, "s", []]})
        };
        WorkloadSchema::from_value(&objects(510)).unwrap();
        assert_eq!(
            WorkloadSchema::from_value(&objects(511))
                .unwrap_err()
                .reason,
            Reason::TooManyValues {
                count: 513,
                limit: 512
            }
        );
    }

    #[test]
    fn strings_count_toward_neither_depth_nor_values() {
        let text = format!("{}{}", "{[".repeat(100), "true false ".repeat(600));
        WorkloadSchema::from_value(&json!({"description": text, "title": "\"{"})).unwrap();
    }
}
