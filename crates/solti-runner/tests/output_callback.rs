use std::{
    env, fmt,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use solti_model::TaskId;
use solti_runner::{
    BuildContext, MetricsBackend, MetricsHandle, OutputPublisher, OutputPublisherHandle,
    OutputSink, RunnerEnv, RunnerErrorKind, RunnerType, record_runner_error, request_output_sink,
};
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

#[test]
fn hostile_callbacks_and_tracing_are_contained_in_subprocess() {
    const CHILD_ENV: &str = "SOLTI_RUNNER_HOSTILE_CALLBACK_CHILD";

    if env::var_os(CHILD_ENV).is_some() {
        run_hostile_callback_child();
        return;
    }

    let test_name = std::thread::current()
        .name()
        .expect("the Rust test harness must name the current test")
        .to_owned();
    let output = Command::new(env::current_exe().expect("the test executable must exist"))
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .output()
        .expect("the hostile-callback child test must start");

    assert!(
        output.status.success(),
        "hostile-callback child failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn run_hostile_callback_child() {
    let reports = Arc::new(AtomicUsize::new(0));
    let retained = Arc::new(());
    tracing::subscriber::set_global_default(HostileSubscriber {
        reports: Arc::clone(&reports),
        retained: Arc::clone(&retained),
    })
    .expect("the isolated child must install its tracing subscriber once");

    let sink_calls = Arc::new(AtomicUsize::new(0));
    let sink_observed = Arc::clone(&sink_calls);
    let sink_retained = Arc::clone(&retained);
    let sink = OutputSink::new(7, 3, move |_| {
        sink_observed.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&sink_retained)));
    });
    let publisher_calls = Arc::new(AtomicUsize::new(0));
    let publisher: OutputPublisherHandle = Arc::new(PanickingPublisher {
        calls: Arc::clone(&publisher_calls),
        retained: Arc::clone(&retained),
    });
    let metrics_calls = Arc::new(AtomicUsize::new(0));
    let metrics: MetricsHandle = Arc::new(PanickingMetrics {
        calls: Arc::clone(&metrics_calls),
        retained: Arc::clone(&retained),
    });
    let ctx = BuildContext::new(RunnerEnv::new(), metrics).with_output_publisher(publisher);
    let task_name = TaskId::new("task").unwrap();

    sink.stdout_line_bytes(b"first");
    assert!(request_output_sink(ctx.output_publisher(), &task_name, 1, 1).is_none());
    record_runner_error(
        ctx.metrics(),
        RunnerType::Subprocess,
        RunnerErrorKind::SpawnFailed,
    );
    let retained_after_first_failures = Arc::strong_count(&retained);

    for attempt in 2..=1_024 {
        sink.stdout_line_bytes(b"later");
        assert!(request_output_sink(ctx.output_publisher(), &task_name, 1, attempt).is_none());
        record_runner_error(
            ctx.metrics(),
            RunnerType::Subprocess,
            RunnerErrorKind::SpawnFailed,
        );
    }

    assert!(sink.callback_panicked());
    assert_eq!(sink_calls.load(Ordering::SeqCst), 1);
    assert_eq!(publisher_calls.load(Ordering::SeqCst), 1);
    assert_eq!(metrics_calls.load(Ordering::SeqCst), 1);
    assert_eq!(reports.load(Ordering::SeqCst), 3);
    assert_eq!(Arc::strong_count(&retained), retained_after_first_failures);
}

struct DestructorPanickingPayload(Arc<()>);

impl Drop for DestructorPanickingPayload {
    fn drop(&mut self) {
        let _ = Arc::strong_count(&self.0);
        panic!("panic payload destructor");
    }
}

struct HostileSubscriber {
    reports: Arc<AtomicUsize>,
    retained: Arc<()>,
}

impl Subscriber for HostileSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, _event: &Event<'_>) {
        self.reports.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

struct PanickingPublisher {
    calls: Arc<AtomicUsize>,
    retained: Arc<()>,
}

impl OutputPublisher for PanickingPublisher {
    fn sink_for(&self, _task_name: &TaskId, _generation: u64, _attempt: u32) -> Option<OutputSink> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
    }
}

struct PanickingMetrics {
    calls: Arc<AtomicUsize>,
    retained: Arc<()>,
}

impl MetricsBackend for PanickingMetrics {
    fn record_runner_error(&self, _runner_type: RunnerType, _error_kind: RunnerErrorKind) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
    }
}
