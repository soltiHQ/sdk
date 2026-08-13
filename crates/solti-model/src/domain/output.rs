//! # Task output
//!
//! [`OutputEvent`] is the shared live-output event.
//! [`OutputChunk`] carries one binary stdout or stderr chunk.
//!
//! This module defines data and serde encoding only.
//! Publishers, channels, retention, and subscriptions belong to higher layers.
//!
//! ## Serde Contract
//!
//! Events use a flat `type` tag.
//! Timestamps use Unix milliseconds.
//! Chunk bytes use standard padded base64.
//!
//! ```text
//! OutputEvent
//!   ├── chunk       ──▶ generation, attempt, stream, seq, ts, line, truncated
//!   ├── runStarted  ──▶ generation, attempt, startedAt
//!   ├── runFinished ──▶ generation, attempt, exitCode, finishedAt
//!   └── lagged      ──▶ skipped, skippedBytes
//! ```
//!
//! `solti-api` maps the same domain events to the separate protobuf shape.

use std::time::SystemTime;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Standard stream that produced a chunk.
///
/// ## Example
///
/// ```
/// use solti_model::StreamKind;
///
/// let json = serde_json::to_string(&StreamKind::Stdout).unwrap();
/// assert_eq!(json, r#""stdout""#);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Standard output (`stdout`).
    Stdout,
    /// Standard error (`stderr`).
    Stderr,
}

/// Event in a task live-output stream.
///
/// Run markers are the best effort.
/// They are not ordering barriers for chunks.
///
/// ## JSON Shape
///
/// ```text
/// {"type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":0,"ts":1700,"line":"aGVsbG8="}
/// {"type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":1,"ts":1701,"line":"aGVsbA==","truncated":true}
/// {"type":"runStarted","generation":2,"attempt":1,"startedAt":1700}
/// {"type":"runFinished","generation":2,"attempt":1,"exitCode":0,"finishedAt":1701}
/// {"type":"lagged","skipped":42,"skippedBytes":65536}
/// ```
///
/// ## Example
///
/// ```
/// use bytes::Bytes;
/// use solti_model::{OutputChunk, OutputEvent, StreamKind};
/// use std::time::SystemTime;
///
/// let event = OutputEvent::Chunk(OutputChunk {
///     generation: 2,
///     attempt: 1,
///     stream: StreamKind::Stdout,
///     seq: 0,
///     ts: SystemTime::UNIX_EPOCH,
///     line: Bytes::from_static(b"hello"),
///     truncated: false,
/// });
///
/// let json = serde_json::to_string(&event).unwrap();
/// assert!(json.contains(r#""type":"chunk""#));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum OutputEvent {
    /// Carries stdout or stderr bytes from one run.
    Chunk(OutputChunk),

    /// Reports that a run attempt started.
    ///
    /// This marker is not an ordering barrier for chunks.
    /// Use each chunk's `generation`, `attempt`, `stream`, and `seq` fields for grouping and ordering.
    #[serde(rename_all = "camelCase")]
    RunStarted {
        /// Desired-state generation executed by this run.
        generation: u64,
        /// Attempt number of the run that just started.
        attempt: u32,
        /// Wall-clock start time (unix milliseconds on the wire).
        #[serde(with = "crate::resource::metadata::time_serde")]
        #[cfg_attr(
            feature = "schema",
            schemars(schema_with = "crate::schema::unix_millis")
        )]
        started_at: SystemTime,
    },

    /// Reports that a run attempt finished.
    ///
    /// This marker is not an ordering barrier: chunks for the same generation and attempt may still be observed after it.
    #[serde(rename_all = "camelCase")]
    RunFinished {
        /// Desired-state generation executed by this run.
        generation: u64,
        /// Attempt number of the run that finished.
        attempt: u32,
        /// Process exit code.
        ///
        /// `None` means no exit code was available.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Wall-clock finish time (unix milliseconds on the wire).
        #[serde(with = "crate::resource::metadata::time_serde")]
        #[cfg_attr(
            feature = "schema",
            schemars(schema_with = "crate::schema::unix_millis")
        )]
        finished_at: SystemTime,
    },

    /// Reports events lost before the next delivered event.
    #[serde(rename_all = "camelCase")]
    Lagged {
        /// Number of lost events.
        skipped: u64,
        /// Retained output bytes carried by the lost events.
        ///
        /// A missing JSON field decodes as zero. Serialization omits zero to
        /// preserve the original wire shape.
        #[serde(default, skip_serializing_if = "is_zero")]
        skipped_bytes: u64,
    },
}

/// Output bytes from one task run.
///
/// ## Example
///
/// ```
/// use bytes::Bytes;
/// use solti_model::{OutputChunk, StreamKind};
/// use std::time::SystemTime;
///
/// let chunk = OutputChunk {
///     generation: 2,
///     attempt: 1,
///     stream: StreamKind::Stderr,
///     seq: 7,
///     ts: SystemTime::UNIX_EPOCH,
///     line: Bytes::from_static(b"warning"),
///     truncated: false,
/// };
///
/// assert_eq!(chunk.seq, 7);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OutputChunk {
    /// Desired-state generation this chunk belongs to.
    pub generation: u64,
    /// Attempt that produced this chunk.
    ///
    /// [`TaskRun::attempt`]: crate::TaskRun::attempt
    pub attempt: u32,
    /// Standard stream that produced the chunk.
    pub stream: StreamKind,
    /// Sequence number within this generation, attempt, and stream.
    ///
    /// Producers define allocation and reset behavior.
    pub seq: u64,
    /// Wall-clock event time.
    ///
    /// Serde encodes it as Unix milliseconds.
    #[serde(with = "crate::resource::metadata::time_serde")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::unix_millis")
    )]
    pub ts: SystemTime,
    /// Exact retained output bytes, excluding a recognized LF or CRLF delimiter.
    ///
    /// Serde encodes them as standard padded base64.
    #[serde(with = "bytes_as_base64")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::base64_bytes")
    )]
    pub line: Bytes,
    /// Whether one or more source bytes were omitted from the end of this line.
    ///
    /// A missing JSON field decodes as `false`. Serialization omits `false` to
    /// preserve the original wire shape.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Serde adapter for exact binary round trips through JSON.
mod bytes_as_base64 {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(b: &Bytes, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&STANDARD.encode(b))
    }

    pub(super) fn deserialize<'de, D>(d: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s)
            .map(Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn wire_shape_is_pinned_for_every_event() {
        let cases = [
            (
                OutputEvent::Chunk(OutputChunk {
                    generation: 2,
                    attempt: 1,
                    stream: StreamKind::Stdout,
                    seq: 0,
                    ts: UNIX_EPOCH + Duration::from_millis(1_700),
                    line: Bytes::from_static(b"hi"),
                    truncated: false,
                }),
                r#"{"type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":0,"ts":1700,"line":"aGk="}"#,
            ),
            (
                OutputEvent::RunStarted {
                    generation: 4,
                    attempt: 2,
                    started_at: UNIX_EPOCH + Duration::from_millis(1_234),
                },
                r#"{"type":"runStarted","generation":4,"attempt":2,"startedAt":1234}"#,
            ),
            (
                OutputEvent::RunFinished {
                    generation: 4,
                    attempt: 2,
                    exit_code: Some(0),
                    finished_at: UNIX_EPOCH + Duration::from_millis(2_222),
                },
                r#"{"type":"runFinished","generation":4,"attempt":2,"exitCode":0,"finishedAt":2222}"#,
            ),
            (
                OutputEvent::Lagged {
                    skipped: 42,
                    skipped_bytes: 0,
                },
                r#"{"type":"lagged","skipped":42}"#,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(serde_json::to_string(&event).unwrap(), expected);
        }
    }

    #[test]
    fn every_event_roundtrips_through_json() {
        let cases = [
            OutputEvent::Chunk(OutputChunk {
                generation: 2,
                attempt: 1,
                stream: StreamKind::Stderr,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
                line: Bytes::from_static(b"warning"),
                truncated: false,
            }),
            OutputEvent::RunStarted {
                generation: 2,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            },
            OutputEvent::RunFinished {
                generation: 2,
                attempt: 1,
                exit_code: Some(42),
                finished_at: UNIX_EPOCH + Duration::from_millis(1_700_000_001_000),
            },
            OutputEvent::Lagged {
                skipped: 7,
                skipped_bytes: 1_024,
            },
        ];

        for original in cases {
            let json = serde_json::to_string(&original).unwrap();
            let back: OutputEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, original, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn binary_chunk_roundtrips_exactly_as_base64() {
        let chunk = OutputChunk {
            generation: 1,
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(&[b'h', b'i', 0xFF, 0xFE]),
            truncated: true,
        };

        let json = serde_json::to_string(&chunk).unwrap();
        assert!(json.contains(r#""line":"aGn//g==""#), "{json}");
        assert!(json.contains(r#""truncated":true"#), "{json}");
        let decoded: OutputChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, chunk);
    }

    #[test]
    fn missing_truncated_field_defaults_to_false() {
        let json = r#"{"generation":1,"attempt":1,"stream":"stdout","seq":0,"ts":0,"line":"/w=="}"#;
        let chunk: OutputChunk = serde_json::from_str(json).unwrap();
        assert_eq!(&chunk.line[..], &[0xff]);
        assert!(!chunk.truncated);
        assert!(!serde_json::to_string(&chunk).unwrap().contains("truncated"));
    }

    #[test]
    fn missing_skipped_bytes_defaults_to_zero() {
        let event: OutputEvent = serde_json::from_str(r#"{"type":"lagged","skipped":3}"#).unwrap();
        assert_eq!(
            event,
            OutputEvent::Lagged {
                skipped: 3,
                skipped_bytes: 0,
            }
        );
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"lagged","skipped":3}"#
        );
    }
}
