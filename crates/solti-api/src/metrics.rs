//! Metrics interface for the API layer (HTTP + gRPC).
//!
//! Implement [`ApiMetricsBackend`] to record per-request metrics.
//! The default is [`NoOpApiMetrics`] - zero-cost when no handle is wired in.
//!
//! Wiring:
//! - HTTP: apply [`http_metrics_middleware`] via [`axum::middleware::from_fn_with_state`]
//!   on the router returned by [`HttpApi::router`](crate::HttpApi::router).
//! - gRPC: construct the service with [`SoltiApiService::new_with_metrics`](crate::SoltiApiService::new_with_metrics)
//!   or call [`build_grpc_server_with_metrics`](crate::build_grpc_server_with_metrics).

use std::sync::Arc;

/// Transport label value — bounded cardinality by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Http,
    Grpc,
}

impl Transport {
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
/// - `method`: HTTP method (`GET`, `POST`, ...) for HTTP, RPC method name (`SubmitTask`, ...) for gRPC
/// - `path`: templated route (`/api/v1/tasks/{id}`) for HTTP via `MatchedPath`, full RPC path (`/solti.v1.SoltiApi/SubmitTask`) for gRPC
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
/// (e.g. `/api/v1/tasks/{id}`) instead of the raw URL — keeps `path` cardinality bounded.
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
        .unwrap_or_else(|| request.uri().path().to_string());

    metrics.record_in_flight_delta(Transport::Http, 1);
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    let duration_ms = start.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    metrics.record_request(Transport::Http, &method, &path, status, duration_ms);
    metrics.record_in_flight_delta(Transport::Http, -1);
    response
}
