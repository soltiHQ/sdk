//! # Workload schemas
//!
//! A runner publishes one JSON Schema for each workload type it runs. Podium
//! names a schema by its fingerprint and validates spec bodies against it.
//! [`WorkloadSchema::from_value`] checks that a schema is a JSON object, writes
//! its RFC 8785 canonical JSON ([`canonical_json`]), which the SDK sends, and
//! refuses canonical JSON over [`MAX_WORKLOAD_SCHEMA_BYTES`].
//!
//! The fingerprint is the lowercase hex SHA-256 of the canonical bytes.
use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use serde::{Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::ModelError;

/// Most bytes the canonical JSON of a workload schema may take.
pub const MAX_WORKLOAD_SCHEMA_BYTES: usize = 64 * 1024;

/// The one `$schema` a workload schema may declare. A schema may also omit it.
pub const WORKLOAD_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Writes any JSON value as RFC 8785 canonical JSON.
///
/// Keys are sorted by their UTF-16 code units, numbers are written as
/// ECMAScript writes the IEEE 754 double they hold, and strings escape only
/// control characters, `"` and `\`.
///
/// # Errors
///
/// Returns [`WorkloadSchemaErrorReason::NotCanonicalizable`] when the value
/// has no canonical form.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, WorkloadSchemaError> {
    serde_json_canonicalizer::to_vec(value).map_err(|err| {
        WorkloadSchemaError::root(WorkloadSchemaErrorReason::NotCanonicalizable(
            err.to_string(),
        ))
    })
}

/// The name of a workload schema: the lowercase hex SHA-256 of its canonical
/// JSON, 64 characters.
///
/// Fingerprints order as their text, which is the order Podium sorts them in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaFingerprint(Arc<str>);

impl SchemaFingerprint {
    const LEN: usize = 64;

    /// Reads a fingerprint.
    ///
    /// # Errors
    ///
    /// Returns [`WorkloadSchemaErrorReason::InvalidFingerprint`] unless the
    /// value is exactly 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, WorkloadSchemaError> {
        let hex = value.len() == Self::LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !hex {
            return Err(WorkloadSchemaError::root(
                WorkloadSchemaErrorReason::InvalidFingerprint {
                    value: excerpt(value),
                },
            ));
        }
        Ok(Self(Arc::from(value)))
    }

    /// The fingerprint as 64 lowercase hex characters.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fingerprint of canonical bytes.
    fn of(canonical: &[u8]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digest = Sha256::digest(canonical);
        let mut text = String::with_capacity(Self::LEN);
        for &byte in digest.iter() {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(Arc::from(text))
    }
}

impl fmt::Display for SchemaFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SchemaFingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A workload schema: its canonical JSON and its fingerprint.
///
/// Two schemas are equal, and hash alike, when their fingerprints are.
/// `Debug` shows the fingerprint and the size, never the schema itself.
#[derive(Clone)]
pub struct WorkloadSchema {
    canonical: Arc<[u8]>,
    fingerprint: SchemaFingerprint,
}

impl WorkloadSchema {
    /// Checks a schema and canonicalizes it.
    ///
    /// It must be a JSON object whose canonical JSON takes at most
    /// [`MAX_WORKLOAD_SCHEMA_BYTES`].
    ///
    /// # Errors
    ///
    /// Returns a [`WorkloadSchemaError`] that names the location of the first
    /// problem found and the rule it breaks.
    pub fn from_value(value: &Value) -> Result<Self, WorkloadSchemaError> {
        if !value.is_object() {
            return Err(WorkloadSchemaError::root(
                WorkloadSchemaErrorReason::NotAnObject,
            ));
        }
        let canonical = canonical_json(value)?;
        if canonical.len() > MAX_WORKLOAD_SCHEMA_BYTES {
            return Err(WorkloadSchemaError::root(
                WorkloadSchemaErrorReason::TooLarge {
                    bytes: canonical.len(),
                    limit: MAX_WORKLOAD_SCHEMA_BYTES,
                },
            ));
        }
        let fingerprint = SchemaFingerprint::of(&canonical);
        Ok(Self {
            canonical: Arc::from(canonical),
            fingerprint,
        })
    }

    /// The RFC 8785 canonical JSON of the schema: the bytes the fingerprint
    /// hashes, and the bytes the SDK sends.
    #[inline]
    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical
    }

    /// The schema's fingerprint.
    #[inline]
    pub fn fingerprint(&self) -> &SchemaFingerprint {
        &self.fingerprint
    }

    /// The schema as a JSON value, read back from its canonical JSON.
    pub fn to_value(&self) -> Value {
        // The canonical bytes are JSON that `serde_json` wrote and read:
        // finite numbers, valid strings, and at most 32 levels deep.
        serde_json::from_slice(&self.canonical).expect("canonical JSON is valid JSON")
    }
}

impl PartialEq for WorkloadSchema {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }
}

impl Eq for WorkloadSchema {}

impl Hash for WorkloadSchema {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

impl fmt::Debug for WorkloadSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkloadSchema")
            .field("fingerprint", &self.fingerprint.as_str())
            .field("bytes", &self.canonical.len())
            .finish()
    }
}

/// A workload schema that the SDK refuses, and where.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("workload schema at {location}: {reason}")]
pub struct WorkloadSchemaError {
    /// Where the problem is, as a URI fragment such as `#/properties/a`: the
    /// subschema whose keyword breaks a rule, or `#` for the whole schema.
    pub location: String,
    /// The rule the schema breaks.
    pub reason: WorkloadSchemaErrorReason,
}

impl WorkloadSchemaError {
    pub(crate) fn new(location: impl Into<String>, reason: WorkloadSchemaErrorReason) -> Self {
        Self {
            location: location.into(),
            reason,
        }
    }

    fn root(reason: WorkloadSchemaErrorReason) -> Self {
        Self::new("#", reason)
    }
}

impl From<WorkloadSchemaError> for ModelError {
    fn from(err: WorkloadSchemaError) -> Self {
        ModelError::Invalid(err.to_string().into())
    }
}

/// The rule a refused workload schema breaks.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum WorkloadSchemaErrorReason {
    /// The schema is not a JSON object.
    #[error("a workload schema must be a JSON object")]
    NotAnObject,

    /// The canonical JSON is over [`MAX_WORKLOAD_SCHEMA_BYTES`].
    #[error("its canonical JSON takes {bytes} bytes, over the limit of {limit}")]
    TooLarge {
        /// Bytes of canonical JSON.
        bytes: usize,
        /// The limit.
        limit: usize,
    },

    /// Text that is not a [`SchemaFingerprint`].
    #[error("{value:?} is not a schema fingerprint: want 64 lowercase hex characters")]
    InvalidFingerprint {
        /// The text, cut to its first 80 bytes.
        value: String,
    },

    /// A value with no RFC 8785 canonical form.
    #[error("it cannot be written as RFC 8785 canonical JSON: {0}")]
    NotCanonicalizable(String),
}

/// Text quoted in an error, cut to its first 80 bytes at a character boundary.
fn excerpt(text: &str) -> String {
    const MAX: usize = 80;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    use crate::{
        ModelError, SchemaFingerprint, WorkloadSchema, WorkloadSchemaErrorReason as Reason,
        canonical_json,
    };

    fn canonical(value: &Value) -> String {
        String::from_utf8(canonical_json(value).unwrap()).unwrap()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn canonical_json_sorts_keys_by_utf16_code_units() {
        // UTF-16 puts U+10000 (D800 DC00) before U+FFFF; UTF-8 would not.
        assert_eq!(
            canonical(&json!({"\u{ffff}": 1, "\u{10000}": 2})),
            "{\"\u{10000}\":2,\"\u{ffff}\":1}"
        );
        assert_eq!(
            canonical(&json!({"b": 1, "a": {"d": 2, "c": 3}, "10": 4, "9": 5, "": 6})),
            r#"{"":6,"10":4,"9":5,"a":{"c":3,"d":2},"b":1}"#
        );
    }

    #[test]
    fn canonical_json_formats_numbers_like_ecmascript() {
        let parsed = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        for (value, expected) in [
            (json!(1e21), "1e+21"),
            (json!(1e-7), "1e-7"),
            (json!(0.000001), "0.000001"),
            (json!(u64::MAX), "18446744073709552000"),
            (json!(9_007_199_254_740_993_u64), "9007199254740992"),
            (json!(-0.0), "0"),
            (json!(1.0), "1"),
            (parsed("333333333.33333329"), "333333333.3333333"),
            (parsed("1E30"), "1e+30"),
            (parsed("4.50"), "4.5"),
            (parsed("0.000000000000000000000000001"), "1e-27"),
        ] {
            assert_eq!(canonical(&value), expected, "{value:?}");
        }
    }

    #[test]
    fn canonical_json_escapes_only_controls_quote_and_backslash() {
        for (value, expected) in [
            ("\u{1}", r#""\u0001""#),
            ("\u{1f}", r#""\u001f""#),
            ("\n", r#""\n""#),
            ("\t\u{8}\u{c}\r", r#""\t\b\f\r""#),
            ("\"\\", r#""\"\\""#),
            ("/", "\"/\""),
            ("é", "\"é\""),
            ("\u{2028}", "\"\u{2028}\""),
            ("\u{7f}", "\"\u{7f}\""),
        ] {
            assert_eq!(canonical(&json!(value)), expected, "{value:?}");
        }
    }

    #[test]
    fn fingerprint_is_the_sha256_of_the_canonical_bytes() {
        let schema = WorkloadSchema::from_value(&json!({"type": "object"})).unwrap();
        assert_eq!(schema.canonical_json(), br#"{"type":"object"}"#);

        let expected = hex(Sha256::digest(br#"{"type":"object"}"#).as_slice());
        assert_eq!(expected.len(), 64);
        assert_eq!(schema.fingerprint().as_str(), expected);
        assert_eq!(schema.fingerprint().to_string(), expected);
    }

    #[test]
    fn fingerprint_ignores_key_order_and_number_spelling() {
        let compact = json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "minimum": 1, "maximum": 100}}
        });
        let spelled: Value = serde_json::from_str(
            r#"{
                "properties": { "n": { "maximum": 1E2, "minimum": 1.0, "type": "integer" } },
                "type": "object"
            }"#,
        )
        .unwrap();

        let a = WorkloadSchema::from_value(&compact).unwrap();
        let b = WorkloadSchema::from_value(&spelled).unwrap();
        assert_eq!(a.canonical_json(), b.canonical_json());
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a, b);

        let other = WorkloadSchema::from_value(&json!({
            "type": "object",
            "properties": {"n": {"type": "integer", "minimum": 2, "maximum": 100}}
        }))
        .unwrap();
        assert_ne!(a.fingerprint(), other.fingerprint());
        assert_ne!(a, other);
    }

    #[test]
    fn schema_must_be_an_object() {
        for value in [json!([]), json!("x"), json!(true), json!(null), json!(1)] {
            let err = WorkloadSchema::from_value(&value).unwrap_err();
            assert_eq!(err.location, "#", "{value}");
            assert_eq!(err.reason, Reason::NotAnObject, "{value}");
        }
    }

    #[test]
    fn fingerprint_parse_takes_only_64_lowercase_hex_characters() {
        let valid = "0123456789abcdef".repeat(4);
        let parsed = SchemaFingerprint::parse(&valid).unwrap();
        assert_eq!(parsed.as_str(), valid);
        assert_eq!(parsed.to_string(), valid);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json!(valid));

        let upper = valid.to_uppercase();
        let short = &valid[..63];
        let long = format!("{valid}0");
        let not_hex = format!("{}g", &valid[..63]);
        for bad in ["", short, long.as_str(), upper.as_str(), not_hex.as_str()] {
            let err = SchemaFingerprint::parse(bad).unwrap_err();
            assert!(
                matches!(err.reason, Reason::InvalidFingerprint { .. }),
                "{bad:?}: {err}"
            );
        }
    }

    #[test]
    fn fingerprints_order_and_hash_as_their_text() {
        let low = SchemaFingerprint::parse(&"0".repeat(64)).unwrap();
        let high = SchemaFingerprint::parse(&"f".repeat(64)).unwrap();
        assert!(low < high);
        let set: HashSet<_> = [low.clone(), low.clone(), high].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&low));
    }

    #[test]
    fn schema_debug_shows_the_fingerprint_and_size_never_the_bytes() {
        let schema = WorkloadSchema::from_value(&json!({"description": "private words"})).unwrap();
        let debug = format!("{schema:?}");
        assert!(debug.contains(schema.fingerprint().as_str()), "{debug}");
        assert!(
            debug.contains(&schema.canonical_json().len().to_string()),
            "{debug}"
        );
        assert!(!debug.contains("private words"), "{debug}");
    }

    #[test]
    fn schemas_compare_and_hash_by_fingerprint() {
        let a = WorkloadSchema::from_value(&json!({"type": "string", "minLength": 1})).unwrap();
        let b = WorkloadSchema::from_value(&json!({"minLength": 1.0, "type": "string"})).unwrap();
        let c = WorkloadSchema::from_value(&json!({"type": "string"})).unwrap();
        let set: HashSet<_> = [a.clone(), b, c].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&a));
    }

    #[test]
    fn to_value_round_trips_to_the_same_fingerprint() {
        let schema = WorkloadSchema::from_value(&json!({
            "type": "number",
            "maximum": 1e21,
            "minimum": 0.000001,
            "default": 1.0
        }))
        .unwrap();
        let value = schema.to_value();
        assert_eq!(
            value,
            json!({"default": 1, "maximum": 1e21, "minimum": 0.000001, "type": "number"})
        );
        assert_eq!(WorkloadSchema::from_value(&value).unwrap(), schema);
    }

    #[test]
    fn errors_display_their_location_and_convert_to_model_errors() {
        let err = WorkloadSchema::from_value(&json!({"properties": {"a": {"$dynamicRef": "#n"}}}))
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("workload schema at #/properties/a: "),
            "{text}"
        );
        assert!(text.contains("$dynamicRef"), "{text}");

        let model = ModelError::from(err);
        assert!(
            matches!(model, ModelError::Invalid(ref message) if message.contains("#/properties/a")),
            "{model}"
        );
    }
}
