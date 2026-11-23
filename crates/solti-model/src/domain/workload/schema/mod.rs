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
