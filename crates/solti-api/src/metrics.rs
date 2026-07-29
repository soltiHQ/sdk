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

/// Keeps the in-flight gauge balanced across every exit path.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) struct InFlightGuard {
    metrics: ApiMetricsHandle,
    transport: Transport,
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl InFlightGuard {
    pub(crate) fn enter(metrics: &ApiMetricsHandle, transport: Transport) -> Self {
        metrics.record_in_flight_delta(transport, 1);
        Self {
            metrics: Arc::clone(metrics),
            transport,
        }
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.record_in_flight_delta(self.transport, -1);
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
        self.metrics.record_request(
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
        atomic::{AtomicI64, Ordering},
    };

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
}
