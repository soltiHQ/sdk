//! # API metrics — HTTP + gRPC.
//!
//! Implement [`ApiMetricsBackend`] to record per-request metrics.
//! The default is [`NoOpApiMetrics`] - zero-cost when no handle is wired in.
//!
//! Wiring:
//! - HTTP: apply [`http_metrics_middleware`] via [`axum::middleware::from_fn_with_state`]
//!   on the router returned by [`HttpApi::router`](crate::HttpApi::router).
//! - gRPC: construct the service with [`TaskApiService::new_with_metrics`](crate::TaskApiService::new_with_metrics)
//!   or chain [`GrpcApi::with_metrics`](crate::GrpcApi::with_metrics).

use std::sync::Arc;

/// Transport that served a request - the `transport` metric label.
///
/// A closed two-value set. This keeps label cardinality bounded by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The axum HTTP/JSON transport (feature `http`).
    Http,
    /// The tonic gRPC transport (feature `grpc`).
    Grpc,
}

impl Transport {
    /// Stable lowercase label value (`"http"` / `"grpc"`) for the metric series.
    pub fn as_label(&self) -> &'static str {
        match self {
            Transport::Http => "http",
            Transport::Grpc => "grpc",
        }
    }
}

/// Metrics backend for the API layer.
///
/// ## Labels
///
/// - `transport`: `http` | `grpc`
/// - `method`: HTTP method (`GET`, `POST`, ...) for HTTP, RPC method name (`CreateTask`, ...) for gRPC
/// - `path`: templated route (`/apis/solti.io/v1/tasks/{name}`) for HTTP via `MatchedPath`, full RPC path (`/solti.task.v1.TaskService/CreateTask`) for gRPC
/// - `status`: HTTP status code (200/404/500/...) for HTTP, gRPC code number for gRPC
///
/// Cardinality stays bounded because routes are a closed set per version and templated paths avoid per-resource-id explosion.
pub trait ApiMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Record a completed request.
    fn record_request(
        &self,
        _transport: Transport,
        _method: &str,
        _path: &str,
        _status: u16,
        _duration_ms: u64,
    ) {
    }

    /// Adjust the in-flight gauge by `delta` (+1 on entry, -1 on exit).
    fn record_in_flight_delta(&self, _transport: Transport, _delta: i64) {}
}

/// Zero-cost default implementation.
#[derive(Debug, Default)]
pub struct NoOpApiMetrics;

impl ApiMetricsBackend for NoOpApiMetrics {}

/// Shareable handle used throughout this crate.
pub type ApiMetricsHandle = Arc<dyn ApiMetricsBackend>;

/// Construct a no-op handle: convenient default.
pub fn noop_api_metrics() -> ApiMetricsHandle {
    Arc::new(NoOpApiMetrics)
}

/// Axum middleware that records per-request HTTP metrics.
///
/// Apply via `axum::middleware::from_fn_with_state(metrics, http_metrics_middleware)`.
///
/// Uses [`axum::extract::MatchedPath`] to capture the route **template**
/// (e.g. `/apis/solti.io/v1/tasks/{name}`) instead of the raw URL. Requests
/// without a matched route use one stable fallback label, keeping `path`
/// cardinality bounded.
#[cfg(feature = "http")]
pub async fn http_metrics_middleware(
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

    metrics.record_in_flight_delta(Transport::Http, 1);
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    let duration_ms = start.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    metrics.record_request(Transport::Http, &method, &path, status, duration_ms);
    metrics.record_in_flight_delta(Transport::Http, -1);
    response
}

#[cfg(all(test, feature = "http"))]
mod tests {
    use std::sync::Mutex;

    use axum::{Router, body::Body, http::Request, middleware};
    use tower::ServiceExt;

    use super::*;

    #[derive(Debug, Default)]
    struct Probe {
        paths: Mutex<Vec<String>>,
    }

    impl ApiMetricsBackend for Probe {
        fn record_request(
            &self,
            _transport: Transport,
            _method: &str,
            path: &str,
            _status: u16,
            _duration_ms: u64,
        ) {
            self.paths.lock().unwrap().push(path.to_string());
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
}
