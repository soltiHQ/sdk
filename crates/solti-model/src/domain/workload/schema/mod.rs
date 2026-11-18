//! # Workload schemas
//!
//! A runner publishes one JSON Schema for each workload type it runs. Podium
//! names a schema by its fingerprint and compiles it with its own draft 2020-12
//! subset (`internal/workloadschema` at commit a1b7ee4). Podium refuses a
//! schema outside that subset without telling the agent, so
//! [`WorkloadSchema::from_value`] refuses, when the runner registers,
//! everything Podium's compiler refuses. It runs, in order:
//!
//! 1. the object check: a schema is a JSON object;
//! 2. canonicalization: the RFC 8785 bytes of [`canonical_json`], which the
//!    fingerprint hashes and the SDK sends;
//! 3. the size of those bytes: at most [`MAX_WORKLOAD_SCHEMA_BYTES`];
//! 4. the shape: objects and arrays nest at most [`MAX_WORKLOAD_SCHEMA_DEPTH`]
//!    levels, and the schema holds at most [`MAX_WORKLOAD_SCHEMA_VALUES`]
//!    objects and booleans;
//! 5. the subset walk: the positions and keyword checks of Podium's compiler,
//!    `$ref` resolution inside the document, `$anchor` and same-location cycles;
//! 6. the regexes: each `pattern` and `patternProperties` key, in the order the
//!    walk met them, against the RE2 syntax that Go and Rust read alike and
//!    against Podium's bounds on regexes, for one regex and for the schema.
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

mod regex;
mod shape;
mod subset;

/// Most bytes the canonical JSON of a workload schema may take.
pub const MAX_WORKLOAD_SCHEMA_BYTES: usize = 64 * 1024;

/// Most levels the objects and arrays of a workload schema may nest; the root
/// object is level 1.
pub const MAX_WORKLOAD_SCHEMA_DEPTH: usize = 32;

/// Most objects and booleans a workload schema may hold, wherever they are.
pub const MAX_WORKLOAD_SCHEMA_VALUES: usize = 512;

/// Most bytes of source one regex of a workload schema may take.
pub const MAX_SCHEMA_REGEX_SOURCE_BYTES: usize = 1024;

/// Most bytes of source the regexes of one workload schema may take together.
pub const MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL: usize = 16 * 1024;

/// Most times one regex may write `\p` or `\P`: Podium's `maxUnicodeClasses`.
///
/// Podium counts each occurrence in the raw text as a Unicode class, even one
/// that is not, such as the `\p` of `\\p`.
pub const MAX_SCHEMA_REGEX_UNICODE_ESCAPES: usize = 15;

/// Most times the regexes of one workload schema may write `\p` or `\P`
/// together.
///
/// Podium charges each 833 ranges of parse work, within 128 000 for a schema:
/// 153 fit, 154 do not.
pub const MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL: usize = 153;

/// Most instructions and class ranges the program of one regex may take, as
/// Podium measures it.
pub const MAX_SCHEMA_REGEX_INSTRUCTIONS: usize = 2_000;

/// Most instructions and class ranges the programs of one workload schema's
/// regexes may take together.
pub const MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL: usize = 20_000;

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

/// A workload schema inside Podium's subset: its canonical JSON and its
/// fingerprint.
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
    /// See the [module docs](self) for the checks, in the order they run.
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
        shape::check_size(&canonical)?;
        shape::check_shape(&canonical)?;
        let patterns = subset::check(value)?;
        let mut budget = regex::RegexBudget::default();
        for pattern in patterns {
            budget
                .charge(pattern.text)
                .map_err(|reason| WorkloadSchemaError::new(pattern.at.to_string(), reason))?;
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

    /// Objects and arrays nest deeper than [`MAX_WORKLOAD_SCHEMA_DEPTH`].
    #[error("its objects and arrays nest deeper than {limit} levels")]
    TooDeep {
        /// The limit.
        limit: usize,
    },

    /// The schema holds more than [`MAX_WORKLOAD_SCHEMA_VALUES`] objects and
    /// booleans.
    #[error("it holds {count} objects and booleans, over the limit of {limit}")]
    TooManyValues {
        /// Objects and booleans in the schema.
        count: usize,
        /// The limit.
        limit: usize,
    },

    /// A draft 2020-12 keyword that the subset leaves out.
    #[error("keyword `{keyword}` is not supported: {why}")]
    UnsupportedKeyword {
        /// The keyword.
        keyword: &'static str,
        /// Why the subset leaves it out.
        why: &'static str,
    },

    /// A keyword of the subset whose value is not what the keyword takes.
    #[error("keyword `{keyword}` must be {expected}")]
    InvalidKeywordValue {
        /// The keyword.
        keyword: &'static str,
        /// What it takes.
        expected: &'static str,
    },

    /// `$schema` names another dialect than [`WORKLOAD_SCHEMA_DIALECT`].
    #[error(
        "`$schema` is {value}: a workload schema is draft 2020-12 \
         (`https://json-schema.org/draft/2020-12/schema`) or has no `$schema`"
    )]
    UnsupportedDialect {
        /// The `$schema` value, as JSON text unless it is a string.
        value: String,
    },

    /// `$id` below the root.
    #[error("`$id` is allowed only at the root of a workload schema")]
    NestedId,

    /// `$ref` to anything outside the document.
    #[error(
        "`$ref` {reference:?} is a remote reference: a workload schema refers \
         only inside itself, by \"#\", \"#/json/pointer\" or \"#anchor\""
    )]
    RemoteReference {
        /// The reference.
        reference: String,
    },

    /// `$ref` that resolves to no subschema of the document.
    #[error("`$ref` {reference:?} does not resolve to a subschema of this document")]
    UnresolvedReference {
        /// The reference.
        reference: String,
    },

    /// An `$anchor` that another subschema declares too.
    #[error("`$anchor` {anchor:?} is declared by another subschema too")]
    DuplicateAnchor {
        /// The anchor.
        anchor: String,
    },

    /// Subschemas that refer back to themselves at the same location of a
    /// body, which no body could be evaluated against.
    #[error(
        "the subschema refers back to itself at the same location of a body (a reference cycle)"
    )]
    ReferenceCycle,

    /// A regex that Podium refuses: Go cannot parse it, or Podium refuses its
    /// syntax. A regex that Rust's `regex-syntax` cannot parse lands here only
    /// when Go's parser refuses it too; otherwise it is
    /// [`SdkStrictRegexRule::GoOnlySyntax`].
    #[error("regex {pattern:?} is refused: {why}")]
    InvalidRegex {
        /// The regex.
        pattern: String,
        /// Why.
        why: String,
    },

    /// A regex the SDK refuses although Podium's Go parser may accept it.
    #[error("regex {pattern:?} is refused by the SDK, although Go may accept it: {rule}")]
    SdkStrictRegex {
        /// The regex.
        pattern: String,
        /// The SDK-strict rule it breaks.
        rule: SdkStrictRegexRule,
    },

    /// A regex over [`MAX_SCHEMA_REGEX_SOURCE_BYTES`].
    #[error("a regex of {bytes} bytes is over the limit of {limit}")]
    RegexTooLong {
        /// Bytes of source.
        bytes: usize,
        /// The limit.
        limit: usize,
    },

    /// A regex that writes `\p` or `\P` more than
    /// [`MAX_SCHEMA_REGEX_UNICODE_ESCAPES`] times.
    #[error("regex {pattern:?} writes `\\p` or `\\P` {count} times, over the limit of {limit}")]
    RegexTooManyUnicodeEscapes {
        /// The regex.
        pattern: String,
        /// Occurrences of `\p` and `\P` in its source.
        count: usize,
        /// The limit.
        limit: usize,
    },

    /// A regex whose program is over [`MAX_SCHEMA_REGEX_INSTRUCTIONS`].
    #[error(
        "regex {pattern:?} compiles to {} instructions and ranges, over the limit of {limit}",
        instructions_text(.instructions)
    )]
    RegexTooLarge {
        /// The regex.
        pattern: String,
        /// Its instructions and class ranges, stopped past the schema's limit.
        instructions: usize,
        /// The limit.
        limit: usize,
    },

    /// The regexes of the schema take more than
    /// [`MAX_SCHEMA_REGEX_SOURCE_BYTES_TOTAL`] bytes of source.
    #[error("the schema's regexes take {bytes} bytes of source, over the limit of {limit} in all")]
    SchemaRegexesTooLong {
        /// Bytes of source so far.
        bytes: usize,
        /// The limit.
        limit: usize,
    },

    /// The regexes of the schema write `\p` or `\P` more than
    /// [`MAX_SCHEMA_REGEX_UNICODE_ESCAPES_TOTAL`] times.
    #[error(
        "the schema's regexes write `\\p` or `\\P` {count} times, over the limit of {limit} in all"
    )]
    SchemaRegexesTooManyUnicodeEscapes {
        /// Occurrences so far.
        count: usize,
        /// The limit.
        limit: usize,
    },

    /// The programs of the schema's regexes take more than
    /// [`MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL`].
    #[error(
        "the schema's regexes compile to {instructions} instructions and ranges, \
         over the limit of {limit} in all"
    )]
    SchemaRegexesTooLarge {
        /// Instructions and class ranges so far.
        instructions: usize,
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

/// Why the SDK refuses a regex that Podium's Go parser may accept.
///
/// These rules are the SDK's own, stricter than Podium on purpose: a pattern
/// that breaks one is refused with
/// [`WorkloadSchemaErrorReason::SdkStrictRegex`], and [`Self::ALL`] lists them.
///
/// Case-insensitive matching never reaches them: every inline flag group,
/// `(?i)` among them, is refused first, as Podium refuses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SdkStrictRegexRule {
    /// `\pL`, `\p{Greek}`, `\P{Han}`: a Unicode property class, whose size in
    /// Podium depends on Go's Unicode tables.
    UnicodeClass,
    /// `\<`, `\>`, `\b{start}`, `\b{end}`, `\b{start-half}` or `\b{end-half}`:
    /// a word boundary in Rust, literal text in Go.
    WordBoundaryExtension,
    /// `&&`, `--` or `~~` inside brackets: a set operation in Rust, literal
    /// characters in Go.
    ClassSetOperation,
    /// A bracketed class that Go reads with other characters or another end,
    /// such as the nested `[b]` of `[a[b]]`, or the leading `--a` of `[--a]`,
    /// a range in Go.
    ClassReadDifferently,
    /// A counted repetition that Go reads as literal text: a space inside the
    /// braces, as in `a{ 2}`, or a count with a leading zero, as in `a{01}`.
    RepetitionReadAsLiteral,
    /// Syntax that Go's parser reads and Rust's `regex-syntax` cannot parse:
    /// `\Q…\E`, an octal escape such as `\0`, a `{` that starts no repetition
    /// (`{`, `x{,5}`), `(?)`, `\x{D800}`, a capture name that starts with a
    /// digit or names two groups, or, inside brackets, an unescaped `[` or a
    /// class escape before `-`, as in `[\d-z]`.
    GoOnlySyntax,
}

impl SdkStrictRegexRule {
    /// Every SDK-strict rule.
    pub const ALL: &'static [Self] = &[
        Self::UnicodeClass,
        Self::WordBoundaryExtension,
        Self::ClassSetOperation,
        Self::ClassReadDifferently,
        Self::RepetitionReadAsLiteral,
        Self::GoOnlySyntax,
    ];
}

impl fmt::Display for SdkStrictRegexRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnicodeClass => {
                "a Unicode property class (\\p…, \\P…) is sized by Go's Unicode tables, \
                 which the SDK cannot reproduce; write explicit ranges instead"
            }
            Self::WordBoundaryExtension => {
                "\\<, \\> and \\b{…} are word boundaries in Rust but literal text in Go"
            }
            Self::ClassSetOperation => {
                "&&, -- and ~~ inside brackets are set operations in Rust but literal \
                 characters in Go"
            }
            Self::ClassReadDifferently => {
                "Go reads this bracketed class differently (a nested [ is literal in Go, \
                 and a leading -- starts a range); escape the character"
            }
            Self::RepetitionReadAsLiteral => {
                "Go reads this counted repetition as literal text; write {n}, {n,} or \
                 {n,m} with no spaces and no leading zeros"
            }
            Self::GoOnlySyntax => {
                "the SDK cannot parse this syntax, although Go may accept it; write it as \
                 both read it: escapes instead of octal and \\Q…\\E, a backslash before a \
                 literal {, [ or -, and capture names that start with a letter and differ"
            }
        })
    }
}

/// A count of instructions for a message: Podium stops counting past the
/// schema's limit.
fn instructions_text(instructions: &usize) -> String {
    if *instructions > MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL {
        format!("more than {MAX_SCHEMA_REGEX_INSTRUCTIONS_TOTAL}")
    } else {
        instructions.to_string()
    }
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
