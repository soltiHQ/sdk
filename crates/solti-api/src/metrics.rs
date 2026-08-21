//! # API Metrics
//!
//! HTTP and gRPC report through one [`ApiMetricsBackend`].
//! Both transport builders use [`NoOpApiMetrics`] by default.
//!
//! ```text
//! HTTP request ──┐
//!                ├──► ApiMetricsBackend
//! gRPC call ─────┘
//! ```
//!
//! Route labels are bounded.
//! HTTP uses matched route templates.
//! gRPC uses full service paths.

use std::sync::Arc;

#[cfg(any(feature = "grpc", feature = "http"))]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(any(feature = "grpc", feature = "http"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "grpc", feature = "http"))]
use std::time::{Duration, Instant};

/// Transport that served an API request.
///
/// This closed set keeps the transport label bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Axum HTTP/JSON transport.
    Http,
    /// Tonic gRPC transport.
    Grpc,
}

impl Transport {
    /// Returns the stable lowercase metric label.
    pub fn as_label(&self) -> &'static str {
        match self {
            Transport::Http => "http",
            Transport::Grpc => "grpc",
        }
    }
}

/// Receives API request lifecycle metrics.
///
/// ## Labels
///
/// | Value       | HTTP                                  | gRPC                                      |
/// |-------------|---------------------------------------|-------------------------------------------|
/// | `method`    | Method such as `GET`                  | RPC name such as `CreateTask`             |
/// | `path`      | Matched route template                | Full service and RPC path                 |
/// | `status`    | Numeric HTTP status                   | Numeric tonic status code                 |
/// | `transport` | [`Transport::Http`]                   | [`Transport::Grpc`]                       |
///
/// `record_request` is called after normal completion or stream termination.
/// It is not called when a request future or stream is dropped first.
/// The in-flight decrement still occurs on drop.
///
/// SDK-owned HTTP and gRPC paths catch unwinding backend panics, discard their opaque payloads,
/// and report the failure without unwinding. API constructors install one sticky boundary around
/// the supplied handle. After its first observed panic, every API path sharing that handle drops
/// later updates without invoking the backend. Direct application calls to these trait methods are
/// not mediated by that boundary. The process panic hook still runs before the unwind is caught. A
/// process built with `panic = "abort"` cannot isolate a backend panic.
/// If destroying a hostile payload itself panics, that replacement payload is intentionally
/// forgotten to prevent another unwind.
/// Calls that already entered the sticky boundary concurrently may still finish or panic. The
/// boundary prevents later invocations; it does not serialize healthy metrics callbacks.
/// Implementations must not panic; SDK containment is a defensive boundary.
pub trait ApiMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Records one completed request or terminated stream.
    fn record_request(
        &self,
        _transport: Transport,
        _method: &str,
        _path: &str,
        _status: u16,
        _duration_ms: u64,
    ) {
    }

    /// Adjusts the in-flight gauge.
    ///
    /// Entry uses `1`.
    /// Completion, failure, cancellation, and drop use `-1`.
    fn record_in_flight_delta(&self, _transport: Transport, _delta: i64) {}
}

/// Metrics backend that ignores every update.
#[derive(Debug, Default)]
pub struct NoOpApiMetrics;

impl ApiMetricsBackend for NoOpApiMetrics {}

/// Shared metrics backend handle.
pub type ApiMetricsHandle = Arc<dyn ApiMetricsBackend>;

/// Creates a shared no-op metrics backend.
pub fn noop_api_metrics() -> ApiMetricsHandle {
    Arc::new(NoOpApiMetrics)
}

/// Installs one sticky panic boundary around an application API metrics backend.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn panic_contained_api_metrics(metrics: ApiMetricsHandle) -> ApiMetricsHandle {
    Arc::new(PanicContainedApiMetrics {
        metrics,
        disabled: AtomicBool::new(false),
    })
}

#[cfg(any(feature = "grpc", feature = "http"))]
struct PanicContainedApiMetrics {
    metrics: ApiMetricsHandle,
    disabled: AtomicBool,
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl std::fmt::Debug for PanicContainedApiMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PanicContainedApiMetrics")
            .field("metrics", &"<backend>")
            .field("disabled", &self.disabled.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl ApiMetricsBackend for PanicContainedApiMetrics {
    fn record_request(
        &self,
        transport: Transport,
        method: &str,
        path: &str,
        status: u16,
        duration_ms: u64,
    ) {
        self.invoke("record_request", || {
            self.metrics
                .record_request(transport, method, path, status, duration_ms);
        });
    }

    fn record_in_flight_delta(&self, transport: Transport, delta: i64) {
        self.invoke("record_in_flight_delta", || {
            self.metrics.record_in_flight_delta(transport, delta);
        });
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl PanicContainedApiMetrics {
    fn invoke(&self, callback: &'static str, invoke: impl FnOnce()) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }

        if let Err(payload) = catch_unwind(AssertUnwindSafe(invoke)) {
            let report = !self.disabled.swap(true, Ordering::AcqRel);
            dispose_panic_payload(payload);
            if report {
                report_without_unwind(|| {
                    tracing::error!(
                        event = "api.metrics_callback_panicked",
                        error_kind = "callback_panicked",
                        callback,
                        "API metrics callback panicked; disabling the installed backend"
                    );
                });
            }
        }
    }
}

/// Keeps the in-flight gauge balanced across every exit path.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) struct InFlightGuard {
    metrics: ApiMetricsHandle,
    transport: Transport,
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl InFlightGuard {
    pub(crate) fn enter(metrics: &ApiMetricsHandle, transport: Transport) -> Self {
        record_in_flight_delta(metrics, transport, 1);
        Self {
            metrics: Arc::clone(metrics),
            transport,
        }
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        record_in_flight_delta(&self.metrics, self.transport, -1);
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) struct RequestMetrics {
    metrics: ApiMetricsHandle,
    transport: Transport,
    method: String,
    path: String,
    started_at: Instant,
    in_flight: Option<InFlightGuard>,
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl RequestMetrics {
    pub(crate) fn enter(
        metrics: &ApiMetricsHandle,
        transport: Transport,
        method: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            metrics: Arc::clone(metrics),
            transport,
            method: method.into(),
            path: path.into(),
            started_at: Instant::now(),
            in_flight: Some(InFlightGuard::enter(metrics, transport)),
        }
    }

    pub(crate) fn complete(&mut self, status: u16) {
        let Some(in_flight) = self.in_flight.take() else {
            return;
        };
        let duration_ms = duration_millis(self.started_at.elapsed());
        record_request(
            &self.metrics,
            self.transport,
            &self.method,
            &self.path,
            status,
            duration_ms,
        );
        drop(in_flight);
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn record_request(
    metrics: &ApiMetricsHandle,
    transport: Transport,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
) {
    invoke_metrics_callback("record_request", || {
        metrics.record_request(transport, method, path, status, duration_ms);
    });
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn record_in_flight_delta(metrics: &ApiMetricsHandle, transport: Transport, delta: i64) {
    invoke_metrics_callback("record_in_flight_delta", || {
        metrics.record_in_flight_delta(transport, delta);
    });
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn invoke_metrics_callback(callback: &'static str, invoke: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(invoke)) {
        dispose_panic_payload(payload);
        report_without_unwind(|| {
            tracing::error!(
                event = "api.metrics_callback_panicked",
                error_kind = "callback_panicked",
                callback,
                "API metrics callback panicked; dropping this update"
            );
        });
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn dispose_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(payload);
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn report_without_unwind(report: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(report)) {
        dispose_panic_payload(payload);
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(feature = "http")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamingResponse;

#[cfg(feature = "http")]
struct HttpMetricsStream {
    inner: axum::body::BodyDataStream,
    request: RequestMetrics,
    status: u16,
}

#[cfg(feature = "http")]
impl tokio_stream::Stream for HttpMetricsStream {
    type Item = Result<axum::body::Bytes, axum::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.inner).poll_next(context) {
            std::task::Poll::Ready(Some(Err(error))) => {
                let status = self.status;
                self.request.complete(status);
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                let status = self.status;
                self.request.complete(status);
                std::task::Poll::Ready(None)
            }
            poll => poll,
        }
    }
}

/// Records HTTP request metrics around the next service.
///
/// Matched routes use their template.
/// Unmatched requests use the stable `<unmatched>` label.
#[cfg(feature = "http")]
pub(crate) async fn http_metrics_middleware(
    axum::extract::State(metrics): axum::extract::State<ApiMetricsHandle>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().as_str().to_string();
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|mp| mp.as_str().to_string())
        .unwrap_or_else(|| "<unmatched>".to_string());

    let mut request_metrics = RequestMetrics::enter(&metrics, Transport::Http, method, path);
    let response = next.run(request).await;
    let status = response.status().as_u16();
    if response.extensions().get::<StreamingResponse>().is_some() {
        let (parts, body) = response.into_parts();
        let stream = HttpMetricsStream {
            inner: body.into_data_stream(),
            request: request_metrics,
            status,
        };
        axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
    } else {
        request_metrics.complete(status);
        response
    }
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    };
    use std::{env, process::Command};

    use axum::{
        Router,
        body::{Body, Bytes},
        http::{Request, StatusCode},
        middleware,
        response::Response,
        routing::get,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[derive(Debug, Default)]
    struct Probe {
        paths: Mutex<Vec<String>>,
        statuses: Mutex<Vec<u16>>,
        in_flight: AtomicI64,
    }

    impl ApiMetricsBackend for Probe {
        fn record_request(
            &self,
            _transport: Transport,
            _method: &str,
            path: &str,
            status: u16,
            _duration_ms: u64,
        ) {
            self.paths.lock().unwrap().push(path.to_string());
            self.statuses.lock().unwrap().push(status);
        }

        fn record_in_flight_delta(&self, _transport: Transport, delta: i64) {
            self.in_flight.fetch_add(delta, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn unmatched_routes_use_one_bounded_path_label() {
        let probe = Arc::new(Probe::default());
        let metrics: ApiMetricsHandle = probe.clone();
        let app = Router::new()
            .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
            .layer(middleware::from_fn_with_state(
                metrics,
                http_metrics_middleware,
            ));

        for path in ["/missing/one", "/missing/two"] {
            app.clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
        }

        let paths = probe.paths.lock().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|path| path == "<unmatched>"));
    }

    #[tokio::test]
    async fn cancellation_releases_http_in_flight_gauge() {
        let probe = Arc::new(Probe::default());
        let metrics: ApiMetricsHandle = probe.clone();
        let app = Router::new()
            .route(
                "/pending",
                get(|| async { std::future::pending::<axum::http::StatusCode>().await }),
            )
            .layer(middleware::from_fn_with_state(
                metrics,
                http_metrics_middleware,
            ));

        let request = Request::builder()
            .uri("/pending")
            .body(Body::empty())
            .unwrap();
        let task = tokio::spawn(app.oneshot(request));
        for _ in 0..100 {
            if probe.in_flight.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);

        task.abort();
        let _ = task.await;
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn streaming_body_error_records_sent_status_once() {
        async fn error_stream() -> Response {
            let stream = tokio_stream::once(Err::<Bytes, std::io::Error>(std::io::Error::other(
                "stream failed",
            )));
            let mut response = Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::from_stream(stream))
                .unwrap();
            response.extensions_mut().insert(StreamingResponse);
            response
        }

        let probe = Arc::new(Probe::default());
        let metrics: ApiMetricsHandle = probe.clone();
        let app = Router::new().route("/stream", get(error_stream)).layer(
            middleware::from_fn_with_state(metrics, http_metrics_middleware),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);
        assert!(probe.paths.lock().unwrap().is_empty());
        assert!(response.into_body().collect().await.is_err());
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(probe.paths.lock().unwrap().len(), 1);
        assert_eq!(
            probe.statuses.lock().unwrap().as_slice(),
            &[StatusCode::ACCEPTED.as_u16()]
        );
    }

    #[test]
    fn installed_metrics_sticky_disable_hostile_callbacks_in_subprocess() {
        const CHILD_ENV: &str = "SOLTI_API_HOSTILE_METRICS_CHILD";

        if env::var_os(CHILD_ENV).is_some() {
            run_hostile_metrics_child();
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
            .expect("the hostile-metrics child test must start");

        assert!(
            output.status.success(),
            "hostile-metrics child failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn run_hostile_metrics_child() {
        let reports = Arc::new(AtomicUsize::new(0));
        let retained = Arc::new(());
        tracing::subscriber::set_global_default(HostileSubscriber {
            reports: Arc::clone(&reports),
            retained: Arc::clone(&retained),
        })
        .expect("the isolated child must install its tracing subscriber once");
        let calls = Arc::new(AtomicUsize::new(0));
        let metrics = panic_contained_api_metrics(Arc::new(PanickingCompletionMetrics {
            calls: Arc::clone(&calls),
            retained: Arc::clone(&retained),
        }));

        let mut request =
            RequestMetrics::enter(&metrics, Transport::Http, "GET", "/hostile-metrics");
        request.complete(StatusCode::OK.as_u16());
        let retained_after_first_failure = Arc::strong_count(&retained);

        for _ in 0..1_024 {
            let mut request =
                RequestMetrics::enter(&metrics, Transport::Http, "GET", "/hostile-metrics");
            request.complete(StatusCode::OK.as_u16());
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(reports.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&retained), retained_after_first_failure);
    }

    #[derive(Debug)]
    struct PanickingCompletionMetrics {
        calls: Arc<AtomicUsize>,
        retained: Arc<()>,
    }

    impl ApiMetricsBackend for PanickingCompletionMetrics {
        fn record_request(
            &self,
            _transport: Transport,
            _method: &str,
            _path: &str,
            _status: u16,
            _duration_ms: u64,
        ) {
            self.panic();
        }

        fn record_in_flight_delta(&self, _transport: Transport, _delta: i64) {
            self.panic();
        }
    }

    impl PanickingCompletionMetrics {
        fn panic(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
        }
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

    impl tracing::Subscriber for HostileSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, _event: &tracing::Event<'_>) {
            self.reports.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }
}
