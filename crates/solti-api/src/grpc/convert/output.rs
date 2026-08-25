//! # Output Conversion
//!
//! Converts live domain output into `StreamTaskLogsResponse`.
//! Raw line bytes move directly into the protobuf message.

use solti_model::{OutputChunk, OutputEvent, StreamKind};

use crate::error::ApiError;
use crate::proto_api;

use super::time::system_time_to_ms;

/// Converts one domain output event into protobuf.
pub(crate) fn output_event_to_proto(
    ev: OutputEvent,
) -> Result<proto_api::StreamTaskLogsResponse, ApiError> {
    use proto_api::stream_task_logs_response::Kind;

    let kind = match ev {
        OutputEvent::Chunk(c) => Kind::Chunk(output_chunk_to_proto(c)?),
        OutputEvent::RunStarted {
            generation,
            attempt,
            started_at,
        } => Kind::RunStarted(proto_api::RunStarted {
            generation,
            attempt,
            started_at: system_time_to_ms(started_at)?,
        }),
        OutputEvent::RunFinished {
            generation,
            attempt,
            exit_code,
            finished_at,
        } => Kind::RunFinished(proto_api::RunFinished {
            generation,
            attempt,
            exit_code,
            finished_at: system_time_to_ms(finished_at)?,
        }),
        OutputEvent::Lagged {
            skipped,
            skipped_bytes,
        } => Kind::Lagged(proto_api::Lagged {
            skipped,
            skipped_bytes,
        }),
        _ => {
            return Err(ApiError::Internal(
                "handler returned an unsupported output event".into(),
            ));
        }
    };

    Ok(proto_api::StreamTaskLogsResponse { kind: Some(kind) })
}

fn output_chunk_to_proto(c: OutputChunk) -> Result<proto_api::OutputChunk, ApiError> {
    Ok(proto_api::OutputChunk {
        generation: c.generation,
        stream: stream_kind_to_proto(c.stream) as i32,
        ts: system_time_to_ms(c.ts)?,
        attempt: c.attempt,
        line: c.line,
        seq: c.seq,
        truncated: c.truncated,
    })
}

fn stream_kind_to_proto(k: StreamKind) -> proto_api::OutputStreamKind {
    match k {
        StreamKind::Stdout => proto_api::OutputStreamKind::Stdout,
        StreamKind::Stderr => proto_api::OutputStreamKind::Stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    use bytes::Bytes;

    #[test]
    fn chunk_maps_all_fields() {
        let line = Bytes::from_static(&[b'e', b'r', b'r', 0xff, 0xfe]);
        let line_ptr = line.as_ptr();
        let ev = OutputEvent::Chunk(OutputChunk {
            generation: 2,
            attempt: 7,
            stream: StreamKind::Stderr,
            seq: 42,
            ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            line,
            truncated: true,
        });

        let proto = output_event_to_proto(ev).unwrap();
        let kind = proto.kind.expect("kind must be set");
        let chunk = match kind {
            proto_api::stream_task_logs_response::Kind::Chunk(c) => c,
            other => panic!("expected Chunk, got {other:?}"),
        };
        assert_eq!(chunk.attempt, 7);
        assert_eq!(chunk.generation, 2);
        assert_eq!(chunk.stream, proto_api::OutputStreamKind::Stderr as i32);
        assert_eq!(chunk.seq, 42);
        assert_eq!(chunk.ts, 1_700_000_000_000);
        assert_eq!(&chunk.line[..], &[b'e', b'r', b'r', 0xff, 0xfe]);
        assert!(chunk.truncated);
        assert_eq!(
            chunk.line.as_ptr(),
            line_ptr,
            "line bytes must be forwarded zero-copy"
        );
    }

    #[test]
    fn lifecycle_events_map_all_fields() {
        match output_event_to_proto(OutputEvent::RunStarted {
            generation: 4,
            attempt: 3,
            started_at: UNIX_EPOCH + Duration::from_millis(1234),
        })
        .unwrap()
        .kind
        .unwrap()
        {
            proto_api::stream_task_logs_response::Kind::RunStarted(r) => {
                assert_eq!(r.attempt, 3);
                assert_eq!(r.generation, 4);
                assert_eq!(r.started_at, 1234);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }

        match output_event_to_proto(OutputEvent::RunFinished {
            generation: 5,
            attempt: 2,
            exit_code: Some(0),
            finished_at: UNIX_EPOCH + Duration::from_millis(2222),
        })
        .unwrap()
        .kind
        .unwrap()
        {
            proto_api::stream_task_logs_response::Kind::RunFinished(r) => {
                assert_eq!(r.attempt, 2);
                assert_eq!(r.generation, 5);
                assert_eq!(r.exit_code, Some(0));
                assert_eq!(r.finished_at, 2222);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }

        match output_event_to_proto(OutputEvent::Lagged {
            skipped: 1500,
            skipped_bytes: 64 * 1024,
        })
        .unwrap()
        .kind
        .unwrap()
        {
            proto_api::stream_task_logs_response::Kind::Lagged(l) => {
                assert_eq!(l.skipped, 1500);
                assert_eq!(l.skipped_bytes, 64 * 1024);
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }
}
