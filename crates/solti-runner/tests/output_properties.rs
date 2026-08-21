use std::sync::{Arc, Mutex};

use bytes::Bytes;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use solti_model::{OutputEvent, StreamKind};
use solti_runner::{OutputChunkRef, OutputSink};

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedChunk {
    generation: u64,
    attempt: u32,
    stream: StreamKind,
    seq: u64,
    line: Vec<u8>,
    truncated: bool,
}

fn owned_recording_sink(
    generation: u64,
    attempt: u32,
) -> (OutputSink, Arc<Mutex<Vec<ObservedChunk>>>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&observed);
    let sink = OutputSink::new(generation, attempt, move |event| {
        let OutputEvent::Chunk(chunk) = event else {
            panic!("OutputSink only publishes chunk events");
        };
        recorded.lock().unwrap().push(ObservedChunk {
            generation: chunk.generation,
            attempt: chunk.attempt,
            stream: chunk.stream,
            seq: chunk.seq,
            line: chunk.line.to_vec(),
            truncated: chunk.truncated,
        });
    });
    (sink, observed)
}

fn borrowed_recording_sink(
    generation: u64,
    attempt: u32,
) -> (OutputSink, Arc<Mutex<Vec<ObservedChunk>>>) {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&observed);
    let sink = OutputSink::new_borrowed(generation, attempt, move |chunk: OutputChunkRef<'_>| {
        recorded.lock().unwrap().push(ObservedChunk {
            generation: chunk.generation(),
            attempt: chunk.attempt(),
            stream: chunk.stream(),
            seq: chunk.seq(),
            line: chunk.line().to_vec(),
            truncated: chunk.truncated(),
        });
    });
    (sink, observed)
}

fn publish_owned(sink: &OutputSink, stream: StreamKind, line: Vec<u8>, truncated: bool) {
    match (stream, truncated) {
        (StreamKind::Stdout, false) => sink.stdout_line(Bytes::from(line)),
        (StreamKind::Stdout, true) => sink.stdout_line_truncated(Bytes::from(line)),
        (StreamKind::Stderr, false) => sink.stderr_line(Bytes::from(line)),
        (StreamKind::Stderr, true) => sink.stderr_line_truncated(Bytes::from(line)),
    }
}

fn publish_borrowed(sink: &OutputSink, stream: StreamKind, line: &[u8], truncated: bool) {
    match (stream, truncated) {
        (StreamKind::Stdout, false) => sink.stdout_line_bytes(line),
        (StreamKind::Stdout, true) => sink.stdout_line_bytes_truncated(line),
        (StreamKind::Stderr, false) => sink.stderr_line_bytes(line),
        (StreamKind::Stderr, true) => sink.stderr_line_bytes_truncated(line),
    }
}

fn framed_lines(line: &[u8]) -> Vec<Vec<u8>> {
    if line.is_empty() {
        return vec![Vec::new()];
    }

    line.split_inclusive(|byte| *byte == b'\n')
        .map(|part| {
            let mut content_end = part.len();
            if part.last() == Some(&b'\n') {
                content_end -= 1;
                if content_end > 0 && part[content_end - 1] == b'\r' {
                    content_end -= 1;
                }
            }
            part[..content_end].to_vec()
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn framing_matches_the_public_binary_contract(
        generation in any::<u64>(),
        attempt in any::<u32>(),
        stdout in any::<bool>(),
        truncated in any::<bool>(),
        line in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let stream = if stdout { StreamKind::Stdout } else { StreamKind::Stderr };
        let expected_lines = framed_lines(&line);
        let (sink, observed) = owned_recording_sink(generation, attempt);

        publish_owned(&sink, stream, line, truncated);

        let observed = observed.lock().unwrap();
        prop_assert_eq!(observed.len(), expected_lines.len());
        for (index, (chunk, expected_line)) in observed.iter().zip(expected_lines).enumerate() {
            prop_assert_eq!(chunk.generation, generation);
            prop_assert_eq!(chunk.attempt, attempt);
            prop_assert_eq!(chunk.stream, stream);
            prop_assert_eq!(chunk.seq, index as u64);
            prop_assert_eq!(&chunk.line, &expected_line);
            prop_assert_eq!(chunk.truncated, truncated && index + 1 == observed.len());
        }
    }

    #[test]
    fn owned_and_borrowed_paths_are_equivalent_and_keep_per_stream_sequences(
        generation in any::<u64>(),
        attempt in any::<u32>(),
        actions in prop::collection::vec(
            (
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
                prop::collection::vec(any::<u8>(), 0..128),
            ),
            0..24,
        ),
    ) {
        let (owned, owned_observed) = owned_recording_sink(generation, attempt);
        let owned_clone = owned.clone();
        let (borrowed, borrowed_observed) = borrowed_recording_sink(generation, attempt);
        let borrowed_clone = borrowed.clone();

        for (stdout, truncated, use_clone, line) in actions {
            let stream = if stdout { StreamKind::Stdout } else { StreamKind::Stderr };
            let owned_target = if use_clone { &owned_clone } else { &owned };
            let borrowed_target = if use_clone { &borrowed_clone } else { &borrowed };
            publish_owned(owned_target, stream, line.clone(), truncated);
            publish_borrowed(borrowed_target, stream, &line, truncated);
        }

        let owned = owned_observed.lock().unwrap().clone();
        let borrowed = borrowed_observed.lock().unwrap().clone();
        prop_assert_eq!(&owned, &borrowed);

        for stream in [StreamKind::Stdout, StreamKind::Stderr] {
            for (expected, chunk) in owned.iter().filter(|chunk| chunk.stream == stream).enumerate() {
                prop_assert_eq!(chunk.seq, expected as u64);
            }
        }
    }
}
