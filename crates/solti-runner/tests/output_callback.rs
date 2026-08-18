use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use solti_runner::OutputSink;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Default)]
struct TraceCapture {
    fields: Mutex<Vec<String>>,
}

struct CaptureSubscriber(Arc<TraceCapture>);

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        event.record(&mut CaptureVisitor(&self.0));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

struct CaptureVisitor<'a>(&'a TraceCapture);

impl Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .fields
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

#[test]
fn callback_panic_is_isolated_reported_once_and_shared_by_clones() {
    const SECRET: &str = "output-callback-secret";

    let capture = Arc::new(TraceCapture::default());
    let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let sink = OutputSink::new(7, 3, move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        panic!("{SECRET}");
    });
    let clone = sink.clone();

    sink.stdout_line(Bytes::from_static(b"first"));
    clone.stderr_line(Bytes::from_static(b"second"));

    assert!(sink.callback_panicked());
    assert!(clone.callback_panicked());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let fields = capture.fields.lock().unwrap().join(" ");
    assert!(
        fields.contains("runner.output_callback_panicked"),
        "{fields}"
    );
    assert!(
        fields.contains("error_kind=\"callback_panicked\""),
        "{fields}"
    );
    assert!(fields.contains("generation=7"), "{fields}");
    assert!(fields.contains("attempt=3"), "{fields}");
    assert!(fields.contains("stream=\"stdout\""), "{fields}");
    assert!(fields.contains("seq=0"), "{fields}");
    assert!(!fields.contains(SECRET), "{fields}");
}
