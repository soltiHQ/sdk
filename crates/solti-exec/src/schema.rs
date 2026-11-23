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
pub(crate) mod testing {}
