//! # `OutputEvent` domain → proto.
//!
//! Maps [`solti_model::OutputEvent`] (in-process broadcast carrier) to the generated [`proto_api::StreamTaskLogsResponse`] for the `StreamTaskLogs` RPC.

use solti_model::{OutputChunk, OutputEvent, StreamKind};

use crate::proto_api;

use super::time::system_time_to_ms;

/// Convert one [`OutputEvent`] into its protobuf representation.
pub(crate) fn output_event_to_proto(ev: OutputEvent) -> proto_api::StreamTaskLogsResponse {
    use proto_api::stream_task_logs_response::Kind;

    let kind = match ev {
        OutputEvent::Chunk(c) => Kind::Chunk(output_chunk_to_proto(c)),
        OutputEvent::RunStarted {
            generation,
            attempt,
            started_at,
        } => Kind::RunStarted(proto_api::RunStarted {
            generation,
            attempt,
            started_at: system_time_to_ms(started_at),
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
            finished_at: system_time_to_ms(finished_at),
        }),
        OutputEvent::Lagged { skipped } => Kind::Lagged(proto_api::Lagged { skipped }),
        _ => return proto_api::StreamTaskLogsResponse { kind: None },
    };

    proto_api::StreamTaskLogsResponse { kind: Some(kind) }
}

fn output_chunk_to_proto(c: OutputChunk) -> proto_api::OutputChunk {
    proto_api::OutputChunk {
        generation: c.generation,
        stream: stream_kind_to_proto(c.stream) as i32,
        ts: system_time_to_ms(c.ts),
        attempt: c.attempt,
        line: c.line,
        seq: c.seq,
    }
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
        let ev = OutputEvent::Chunk(OutputChunk {
            generation: 2,
            attempt: 7,
            stream: StreamKind::Stderr,
            seq: 42,
            ts: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            line: Bytes::from_static(b"error: boom"),
        });

        let proto = output_event_to_proto(ev);
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
        assert_eq!(&chunk.line[..], b"error: boom");
    }

    #[test]
    fn chunk_forwards_line_without_byte_copy() {
        let original = Bytes::from_static(b"shared-line");
        let original_ptr = original.as_ptr();

        let ev = OutputEvent::Chunk(OutputChunk {
            generation: 1,
            attempt: 1,
            stream: StreamKind::Stdout,
            seq: 0,
            ts: UNIX_EPOCH,
            line: original,
        });

        let proto = output_event_to_proto(ev);
        let chunk = match proto.kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::Chunk(c) => c,
            other => panic!("expected Chunk, got {other:?}"),
        };
        assert_eq!(
            chunk.line.as_ptr(),
            original_ptr,
            "line bytes must be forwarded zero-copy"
        );
    }

    #[test]
    fn run_started_maps_attempt_and_ts() {
        let ev = OutputEvent::RunStarted {
            generation: 4,
            attempt: 3,
            started_at: UNIX_EPOCH + Duration::from_millis(1234),
        };
        match output_event_to_proto(ev).kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunStarted(r) => {
                assert_eq!(r.attempt, 3);
                assert_eq!(r.generation, 4);
                assert_eq!(r.started_at, 1234);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[test]
    fn run_finished_carries_exit_code_and_ts() {
        let ev = OutputEvent::RunFinished {
            generation: 5,
            attempt: 2,
            exit_code: Some(0),
            finished_at: UNIX_EPOCH + Duration::from_millis(2222),
        };
        match output_event_to_proto(ev).kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunFinished(r) => {
                assert_eq!(r.attempt, 2);
                assert_eq!(r.generation, 5);
                assert_eq!(r.exit_code, Some(0));
                assert_eq!(r.finished_at, 2222);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[test]
    fn lagged_carries_skipped_count() {
        let ev = OutputEvent::Lagged { skipped: 1500 };
        match output_event_to_proto(ev).kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::Lagged(l) => {
                assert_eq!(l.skipped, 1500);
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }
}
