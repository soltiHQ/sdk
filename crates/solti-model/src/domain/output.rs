//! Output streaming types for live tail of task stdout/stderr.
//!
//! ## Wire encodings (contract)
//!
//! The same events cross the API boundary in two encodings, and they are **different by design**:
//!
//! | Transport | Encoding                                                          | Source of truth                           |
//! |-----------|-------------------------------------------------------------------|-------------------------------------------|
//! | HTTP SSE  | this module's serde (`type`-tagged camelCase JSON, ms timestamps) | [`OutputEvent`] derives                   |
//! | gRPC      | proto `StreamTaskLogsResponse` (binary protobuf)                  | `solti-api/proto/solti/task/v1/api.proto` |
//!
//! Do not switch the SSE path to pbjson-generated JSON:
//! the shapes differ (`oneof` nesting vs flat `type` tag) and existing SSE consumers parse this module's shape.
//! A pinning test below locks the SSE encoding.

use std::time::SystemTime;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Which standard stream a chunk came from.
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
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Standard output (`stdout`).
    Stdout,
    /// Standard error (`stderr`).
    Stderr,
}

/// One event in the live-tail stream of a task.
///
/// Carries either an output line, a best-effort run marker, or a lag/loss notification.
/// Wire format is JSON-tagged on `type`:
///
/// ```text
/// {"type":"chunk","attempt":1,"stream":"stdout","seq":0,"ts":1700,"line":"..."}
/// {"type":"runStarted","attempt":1,"startedAt":1700}
/// {"type":"runFinished","attempt":1,"exitCode":0,"finishedAt":1701}
/// {"type":"lagged","skipped":42}
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
///     attempt: 1,
///     stream: StreamKind::Stdout,
///     seq: 0,
///     ts: SystemTime::UNIX_EPOCH,
///     line: Bytes::from_static(b"hello"),
/// });
///
/// let json = serde_json::to_string(&event).unwrap();
/// assert!(json.contains(r#""type":"chunk""#));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum OutputEvent {
    /// One line of stdout/stderr from the currently active run.
    Chunk(OutputChunk),

    /// Best-effort observation that a new run attempt has started.
    ///
    /// This marker is not an ordering barrier for chunks. Use each chunk's
    /// `attempt`, `stream`, and `seq` fields for grouping and ordering.
    #[serde(rename_all = "camelCase")]
    RunStarted {
        /// Attempt number of the run that just started.
        attempt: u32,
        /// Wall-clock start time (unix milliseconds on the wire).
        #[serde(with = "crate::resource::metadata::time_serde")]
        started_at: SystemTime,
    },

    /// Best-effort observation that a run attempt has finished.
    ///
    /// This marker is not an ordering barrier: chunks for the same attempt may
    /// still be observed after it.
    #[serde(rename_all = "camelCase")]
    RunFinished {
        /// Attempt number of the run that finished.
        attempt: u32,
        /// Process exit code. `None` when the run ended without one (killed, canceled).
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Wall-clock finish time (unix milliseconds on the wire).
        #[serde(with = "crate::resource::metadata::time_serde")]
        finished_at: SystemTime,
    },

    /// Subscriber fell behind the broadcast ring window.
    Lagged {
        /// Number of events dropped before the subscriber caught up.
        skipped: u64,
    },
}

/// One line of output from a single task-run attempt.
///
/// Carried through `tokio::sync::broadcast` channels in-process;
/// sent to clients via SSE / gRPC server-stream.
///
/// ## Example
///
/// ```
/// use bytes::Bytes;
/// use solti_model::{OutputChunk, StreamKind};
/// use std::time::SystemTime;
///
/// let chunk = OutputChunk {
///     attempt: 1,
///     stream: StreamKind::Stderr,
///     seq: 7,
///     ts: SystemTime::UNIX_EPOCH,
///     line: Bytes::from_static(b"warning"),
/// };
///
/// assert_eq!(chunk.seq, 7);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputChunk {
    /// Which attempt of the task this chunk belongs to (matches [`TaskRun::attempt`]).
    ///
    /// [`TaskRun::attempt`]: crate::TaskRun::attempt
    pub attempt: u32,
    /// stdout or stderr.
    pub stream: StreamKind,
    /// Monotonic sequence number per stream within this attempt.
    ///
    /// `stdout` and `stderr` have independent counters, each reset for a new attempt.
    pub seq: u64,
    /// Wall-clock time the line was read by the agent (unix milliseconds on the wire).
    #[serde(with = "crate::resource::metadata::time_serde")]
    pub ts: SystemTime,
    /// One line, truncated to the runner's configured limit.
    ///
    /// Live-tail payloads are not sanitized and may contain control characters.
    #[serde(with = "bytes_as_utf8_string")]
    pub line: Bytes,
}

/// Serde adapter: serialize `Bytes` as a UTF-8 string in JSON, deserialize from a JSON string back into `Bytes`.
mod bytes_as_utf8_string {
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(b: &Bytes, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match std::str::from_utf8(b) {
            Ok(txt) => s.serialize_str(txt),
            Err(_) => s.serialize_str(&String::from_utf8_lossy(b)),
        }
    }

    pub(super) fn deserialize<'de, D>(d: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        Ok(Bytes::from(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn stream_kind_stdout_serializes_to_lowercase() {
        let json = serde_json::to_string(&StreamKind::Stdout).unwrap();
        assert_eq!(json, "\"stdout\"");
    }

    #[test]
    fn sse_wire_shape_is_pinned() {
        let chunk = OutputEvent::Chunk(OutputChunk {
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH + Duration::from_millis(1_700),
            line: Bytes::from_static(b"hi"),
        });
        assert_eq!(
            serde_json::to_string(&chunk).unwrap(),
            r#"{"type":"chunk","attempt":1,"stream":"stdout","seq":0,"ts":1700,"line":"hi"}"#
        );

        let lagged = OutputEvent::Lagged { skipped: 42 };
        assert_eq!(
            serde_json::to_string(&lagged).unwrap(),
            r#"{"type":"lagged","skipped":42}"#
        );
    }

    #[test]
    fn output_chunk_roundtrips_through_json() {
        let chunk = OutputChunk {
            attempt: 7,
            stream: StreamKind::Stderr,
            seq: 42,
            ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            line: Bytes::from_static(b"compiling foo..."),
        };

        let json = serde_json::to_string(&chunk).unwrap();
        let back: OutputChunk = serde_json::from_str(&json).unwrap();

        assert_eq!(back, chunk);
    }

    #[test]
    fn output_chunk_serializes_ts_as_unix_milliseconds() {
        let chunk = OutputChunk {
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH + Duration::from_millis(1234),
            line: Bytes::from_static(b"x"),
        };

        let json = serde_json::to_string(&chunk).unwrap();
        assert!(
            json.contains(r#""ts":1234"#),
            "ts must serialize as unix milliseconds; got {json}"
        );
    }

    #[test]
    fn output_chunk_serializes_line_as_utf8_string_not_array() {
        let chunk = OutputChunk {
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(b"hello"),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(
            json.contains(r#""line":"hello""#),
            "line must serialize as JSON string, not byte array; got {json}"
        );
    }

    #[test]
    fn output_event_chunk_inlines_chunk_fields() {
        let event = OutputEvent::Chunk(OutputChunk {
            attempt: 3,
            stream: StreamKind::Stdout,
            seq: 5,
            ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            line: Bytes::from_static(b"hello"),
        });
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains(r#""type":"chunk""#), "missing tag in {json}");
        assert!(json.contains(r#""attempt":3"#), "{json}");
        assert!(json.contains(r#""stream":"stdout""#), "{json}");
        assert!(json.contains(r#""line":"hello""#), "{json}");
    }

    #[test]
    fn output_event_run_started_carries_attempt_and_ts() {
        let event = OutputEvent::RunStarted {
            attempt: 2,
            started_at: UNIX_EPOCH + Duration::from_millis(1234),
        };
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains(r#""type":"runStarted""#), "{json}");
        assert!(json.contains(r#""attempt":2"#), "{json}");
        assert!(json.contains(r#""startedAt":1234"#), "{json}");
    }

    #[test]
    fn output_event_run_finished_carries_exit_code() {
        let event = OutputEvent::RunFinished {
            attempt: 2,
            exit_code: Some(0),
            finished_at: UNIX_EPOCH + Duration::from_millis(2222),
        };
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains(r#""type":"runFinished""#), "{json}");
        assert!(json.contains(r#""exitCode":0"#), "{json}");
        assert!(json.contains(r#""finishedAt":2222"#), "{json}");
    }

    #[test]
    fn output_event_lagged_carries_skipped_count() {
        let event = OutputEvent::Lagged { skipped: 1500 };
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains(r#""type":"lagged""#), "{json}");
        assert!(json.contains(r#""skipped":1500"#), "{json}");
    }

    #[test]
    fn output_event_roundtrips_through_json() {
        let cases = [
            OutputEvent::Chunk(OutputChunk {
                attempt: 1,
                stream: StreamKind::Stderr,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
                line: Bytes::from_static(b"warning"),
            }),
            OutputEvent::RunStarted {
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            },
            OutputEvent::RunFinished {
                attempt: 1,
                exit_code: Some(42),
                finished_at: UNIX_EPOCH + Duration::from_millis(1_700_000_001_000),
            },
            OutputEvent::Lagged { skipped: 7 },
        ];

        for original in cases {
            let json = serde_json::to_string(&original).unwrap();
            let back: OutputEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, original, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn output_chunk_uses_camel_case_keys() {
        let chunk = OutputChunk {
            attempt: 2,
            stream: StreamKind::Stdout,
            seq: 9,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(b"hi"),
        };

        let json = serde_json::to_string(&chunk).unwrap();
        for key in [
            r#""attempt":"#,
            r#""stream":"#,
            r#""seq":"#,
            r#""ts":"#,
            r#""line":"#,
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
    }

    #[test]
    fn output_chunk_clone_is_refcount_bump() {
        let original = OutputChunk {
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(b"shared-line"),
        };
        let cloned = original.clone();
        assert_eq!(original.line.as_ptr(), cloned.line.as_ptr());
    }

    #[test]
    fn output_chunk_with_non_utf8_line_serializes_lossily_instead_of_failing() {
        let chunk = OutputChunk {
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: Bytes::from_static(&[b'h', b'i', 0xFF, 0xFE]),
        };

        let json = serde_json::to_string(&chunk)
            .expect("non-UTF8 line must serialize lossily, not error out");

        assert!(json.contains("hi"), "valid prefix must survive: {json}");
        assert!(
            json.contains('\u{FFFD}'),
            "invalid bytes must be replaced with U+FFFD, not dropped: {json}"
        );
    }
}
