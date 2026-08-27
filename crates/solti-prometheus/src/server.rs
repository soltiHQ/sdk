//! # Metrics server
//!
//! [`server`] and [`server_with_config`] build an embedded task for a Prometheus endpoint.
//! The task reads one shared [`Registry`].
//!
//! Enable it with the `server` feature.
//!
//! ## Flow
//!
//! ```text
//! Registry + address + revision
//!              ▼
//!   server() / server_with_config()
//!              ├──► TaskManifest
//!              └──► TaskRef ──► bind address ──► GET /metrics
//! ```
//!
//! `server()` does not bind the address.
//! Binding starts when Taskvisor runs the returned task.
//!
//! The server exposes a plaintext, unauthenticated `GET /metrics` endpoint.
//! Production deployments must restrict its reachability with a controlled
//! bind address, network policy, firewall, or authenticated TLS proxy.

use std::{
    convert::Infallible,
    fmt, io,
    io::Write,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use prometheus::{Encoder, Registry, TextEncoder};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy, TaskManifest,
    TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::Stream;
use tracing::{debug, error, info, warn};

/// Slot used by the embedded metrics server.
///
/// ## Example
///
/// ```
/// assert_eq!(solti_prometheus::METRICS_SERVER_SLOT, "solti-metrics-server");
/// ```
pub const METRICS_SERVER_SLOT: &str = "solti-metrics-server";

/// Per-attempt timeout used by the task specification.
const METRICS_SERVER_TIMEOUT_MS: u64 = u64::MAX;

/// Prometheus text exposition content-type (format version 0.0.4).
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// First amortized allocation target for an encoded response.
const INITIAL_RESPONSE_CAPACITY: usize = 4 * 1024;

/// Initial backoff delay on failure in milliseconds.
const BACKOFF_FIRST_MS: u64 = 1_000;

/// Maximum backoff delay on repeated failures in milliseconds.
const BACKOFF_MAX_MS: u64 = 30_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Bounded `/metrics` scrape settings.
///
/// The limits apply to physical gather jobs and the encoded HTTP body.
/// A timed-out gather job keeps its concurrency slot until the blocking
/// collector work physically returns.
/// A successful gather transfers that slot to the encoded response bytes and
/// releases it only when their final owner is dropped.
///
/// These settings cannot limit allocations or blocking performed inside an
/// arbitrary [`prometheus::core::Collector`]. Applications must register only
/// trusted collectors with their own bounded collection behavior. Collector
/// work is concurrency-bounded, not byte-bounded.
///
/// ## Example
///
/// ```
/// use std::time::Duration;
/// use solti_prometheus::MetricsServerConfig;
///
/// let config = MetricsServerConfig::new()
///     .try_with_max_concurrent_scrapes(4)?
///     .try_with_max_response_bytes(8 * 1024 * 1024)?
///     .try_with_scrape_timeout(Duration::from_secs(15))?;
///
/// assert_eq!(config.max_concurrent_scrapes().get(), 4);
/// # Ok::<(), solti_prometheus::MetricsServerConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsServerConfig {
    max_concurrent_scrapes: NonZeroUsize,
    max_response_bytes: NonZeroUsize,
    scrape_timeout: Duration,
}

impl MetricsServerConfig {
    /// Default maximum number of concurrent scrape ownerships.
    pub const DEFAULT_MAX_CONCURRENT_SCRAPES: NonZeroUsize = NonZeroUsize::new(2).unwrap();
    /// Hard maximum number of concurrent scrape ownerships.
    pub const MAX_CONCURRENT_SCRAPES: NonZeroUsize = NonZeroUsize::new(16).unwrap();
    /// Default maximum encoded response size: 4 MiB.
    pub const DEFAULT_MAX_RESPONSE_BYTES: NonZeroUsize =
        NonZeroUsize::new(4 * 1024 * 1024).unwrap();
    /// Hard maximum encoded response size: 64 MiB.
    pub const MAX_RESPONSE_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024 * 1024).unwrap();
    /// Default HTTP response deadline for gather and encoding.
    pub const DEFAULT_SCRAPE_TIMEOUT: Duration = Duration::from_secs(10);
    /// Minimum HTTP response deadline for gather and encoding.
    pub const MIN_SCRAPE_TIMEOUT: Duration = Duration::from_millis(1);
    /// Hard maximum HTTP response deadline for gather and encoding.
    pub const MAX_SCRAPE_TIMEOUT: Duration = Duration::from_secs(60);

    /// Creates the default bounded scrape settings.
    pub const fn new() -> Self {
        Self {
            max_concurrent_scrapes: Self::DEFAULT_MAX_CONCURRENT_SCRAPES,
            max_response_bytes: Self::DEFAULT_MAX_RESPONSE_BYTES,
            scrape_timeout: Self::DEFAULT_SCRAPE_TIMEOUT,
        }
    }

    /// Returns the maximum number of concurrent scrape ownerships.
    pub const fn max_concurrent_scrapes(self) -> NonZeroUsize {
        self.max_concurrent_scrapes
    }

    /// Returns the maximum encoded response size.
    pub const fn max_response_bytes(self) -> NonZeroUsize {
        self.max_response_bytes
    }

    /// Returns the HTTP response deadline for gather and encoding.
    pub const fn scrape_timeout(self) -> Duration {
        self.scrape_timeout
    }

    /// Replaces the maximum number of concurrent scrape ownerships.
    ///
    /// # Errors
    ///
    /// Returns [`MetricsServerConfigError::MaxConcurrentScrapesOutOfRange`]
    /// unless `value` is in `1..=16`.
    pub const fn try_with_max_concurrent_scrapes(
        mut self,
        value: usize,
    ) -> Result<Self, MetricsServerConfigError> {
        let Some(value) = NonZeroUsize::new(value) else {
            return Err(MetricsServerConfigError::MaxConcurrentScrapesOutOfRange { value });
        };
        if value.get() > Self::MAX_CONCURRENT_SCRAPES.get() {
            return Err(MetricsServerConfigError::MaxConcurrentScrapesOutOfRange {
                value: value.get(),
            });
        }
        self.max_concurrent_scrapes = value;
        Ok(self)
    }

    /// Replaces the maximum encoded response size.
    ///
    /// # Errors
    ///
    /// Returns [`MetricsServerConfigError::MaxResponseBytesOutOfRange`] unless
    /// `value` is in `1..=64 MiB`.
    pub const fn try_with_max_response_bytes(
        mut self,
        value: usize,
    ) -> Result<Self, MetricsServerConfigError> {
        let Some(value) = NonZeroUsize::new(value) else {
            return Err(MetricsServerConfigError::MaxResponseBytesOutOfRange { value });
        };
        if value.get() > Self::MAX_RESPONSE_BYTES.get() {
            return Err(MetricsServerConfigError::MaxResponseBytesOutOfRange {
                value: value.get(),
            });
        }
        self.max_response_bytes = value;
        Ok(self)
    }

    /// Replaces the HTTP response deadline for gather and encoding.
    ///
    /// # Errors
    ///
    /// Returns [`MetricsServerConfigError::ScrapeTimeoutOutOfRange`] unless
    /// `value` is in `1 ms..=60 s`.
    pub fn try_with_scrape_timeout(
        mut self,
        value: Duration,
    ) -> Result<Self, MetricsServerConfigError> {
        if !(Self::MIN_SCRAPE_TIMEOUT..=Self::MAX_SCRAPE_TIMEOUT).contains(&value) {
            return Err(MetricsServerConfigError::ScrapeTimeoutOutOfRange { value });
        }
        self.scrape_timeout = value;
        Ok(self)
    }
}

impl Default for MetricsServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Invalid [`MetricsServerConfig`] override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricsServerConfigError {
    /// The scrape-ownership limit is zero or above the hard ceiling.
    MaxConcurrentScrapesOutOfRange {
        /// Rejected value.
        value: usize,
    },
    /// The encoded-response limit is zero or above the hard ceiling.
    MaxResponseBytesOutOfRange {
        /// Rejected value.
        value: usize,
    },
    /// The scrape response deadline is below one millisecond or above the hard ceiling.
    ScrapeTimeoutOutOfRange {
        /// Rejected value.
        value: Duration,
    },
}

impl fmt::Display for MetricsServerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxConcurrentScrapesOutOfRange { value } => write!(
                formatter,
                "max_concurrent_scrapes must be in 1..={}, got {value}",
                MetricsServerConfig::MAX_CONCURRENT_SCRAPES
            ),
            Self::MaxResponseBytesOutOfRange { value } => write!(
                formatter,
                "max_response_bytes must be in 1..={}, got {value}",
                MetricsServerConfig::MAX_RESPONSE_BYTES
            ),
            Self::ScrapeTimeoutOutOfRange { value } => write!(
                formatter,
                "scrape_timeout must be in {:?}..={:?}, got {value:?}",
                MetricsServerConfig::MIN_SCRAPE_TIMEOUT,
                MetricsServerConfig::MAX_SCRAPE_TIMEOUT
            ),
        }
    }
}

impl std::error::Error for MetricsServerConfigError {}

/// Builds a supervised metrics-server task.
///
/// The returned [`TaskManifest`] and [`TaskRef`] form one embedded task.
/// Submit both through the `solti-core` embedded-task API.
///
/// ## Runtime Flow
///
/// ```text
/// Taskvisor attempt
///       │
///       ├── bind failure ─────────────► TaskError::Fail
///       ├── serve failure ────────────► TaskError::Fail
///       ├── cancellation ─────────────► graceful shutdown
///       └── GET /metrics ──► gather ──► Prometheus text
/// ```
///
/// ## Task Settings
///
/// | Setting         | Value                                      |
/// |-----------------|--------------------------------------------|
/// | Slot            | [`METRICS_SERVER_SLOT`]                    |
/// | Workload        | Embedded                                   |
/// | Restart         | Always, without an interval                |
/// | Failure backoff | 1s to 30s, factor `2`, equal jitter        |
/// | Admission       | [`AdmissionPolicy::Replace`]               |
/// | Attempt timeout | `u64::MAX` milliseconds                    |
/// | Scrape slots    | `2`                                        |
/// | Response limit  | 4 MiB                                      |
/// | HTTP deadline   | 10 seconds                                 |
///
/// The composed embedded revision contains the caller revision, listen address,
/// and default scrape settings. Changing any effective value changes the revision.
///
/// The endpoint is plaintext and unauthenticated. Binding `0.0.0.0` exposes it
/// on every available interface. Production deployments must provide an
/// appropriate network perimeter.
///
/// ## Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use solti_prometheus::{Registry, server};
///
/// let registry = Arc::new(Registry::new());
/// // ... register collectors into `registry` ...
///
/// let (manifest, task_ref) = server(
///     registry.clone(),
///     "0.0.0.0:9090",
///     "my-agent@v1",
/// )?;
/// // Submit to a running supervisor:
/// // supervisor.create_embedded_task(manifest, task_ref).await?;
/// # let _ = (manifest, task_ref);
/// # Ok::<(), solti_model::ModelError>(())
/// ```
///
/// # Errors
///
/// Returns [`solti_model::ModelError`] when the caller revision is invalid.
/// It also returns this error when the composed task specification is invalid.
///
/// Address parsing and binding happen inside the task.
/// A bind failure becomes a retryable [`TaskError`].
pub fn server(
    registry: Arc<Registry>,
    addr: impl Into<String>,
    revision: impl Into<String>,
) -> Result<(TaskManifest, TaskRef), solti_model::ModelError> {
    server_with_config(registry, addr, revision, MetricsServerConfig::default())
}

/// Builds a supervised metrics-server task with explicit bounded scrape settings.
///
/// This function has the same lifecycle contract as [`server`]. The composed
/// embedded revision additionally covers every effective [`MetricsServerConfig`]
/// value.
///
/// The endpoint is plaintext and unauthenticated. Production deployments must
/// restrict its reachability outside this server API.
///
/// A request receives `503 Service Unavailable` with `Retry-After: 1` when all
/// scrape ownership slots are occupied. It receives `504 Gateway Timeout` when its
/// response deadline elapses. Encoding failures, collector panics, and response
/// size violations return `500 Internal Server Error`. A response that exceeds
/// the configured byte limit is rejected in full; no truncated Prometheus
/// exposition is sent.
///
/// Gather and encoding run on Tokio's blocking pool. A timed-out or disconnected
/// request does not cancel arbitrary synchronous collector code. That physical
/// job retains its gather slot until it returns. A completed response transfers
/// the slot into the encoded [`Bytes`], including clones held by the transport.
/// Collector and temporary encoder allocations remain outside the response-byte
/// ceiling.
/// An unwinding scrape panic becomes `500`; a process built with `panic = "abort"`
/// cannot isolate it. Structured tracing is best effort. A tracing subscriber
/// panic is contained and cannot replace the scrape outcome.
/// If a user-defined panic payload panics again from its destructor, the
/// replacement payload is forgotten to prevent a second unwind from escaping.
/// Repeated custom payloads with panicking destructors can therefore leak one
/// replacement payload per admitted panic.
///
/// # Errors
///
/// Returns [`solti_model::ModelError`] when the caller revision is invalid.
/// It also returns this error when the composed task specification is invalid.
pub fn server_with_config(
    registry: Arc<Registry>,
    addr: impl Into<String>,
    revision: impl Into<String>,
    config: MetricsServerConfig,
) -> Result<(TaskManifest, TaskRef), solti_model::ModelError> {
    let addr: String = addr.into();
    let caller_revision = EmbeddedSpec::new(revision)?;
    let revision = format!(
        "{}|addr={addr}|max_concurrent_scrapes={}|max_response_bytes={}|scrape_timeout_ns={}",
        caller_revision.revision(),
        config.max_concurrent_scrapes(),
        config.max_response_bytes(),
        config.scrape_timeout().as_nanos(),
    );
    let server_state = Arc::new(MetricsServerState::new(registry, config));

    let task: TaskRef = TaskFn::arc(move |ctx: TaskContext| {
        let addr = addr.clone();
        let server_state = Arc::clone(&server_state);
        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }

            let app = metrics_router(server_state);

            let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
                TaskError::fail(format!("metrics listener bind failed on {addr}: {e}"))
            })?;
            report_without_unwind(|| {
                info!(
                    event = "metrics.server_started",
                    listen_addr = %addr,
                    path = "/metrics",
                    "metrics server started"
                );
            });

            let shutdown_ctx = ctx.clone();
            let serve_result = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_ctx.cancelled().await })
                .await;

            if ctx.is_cancelled() {
                report_without_unwind(|| {
                    debug!(
                        event = "metrics.server_stopped",
                        reason = "canceled",
                        "metrics server stopped"
                    );
                });
                return Err(TaskError::Canceled);
            }

            Err(TaskError::fail(match serve_result {
                Ok(()) => "metrics server exited unexpectedly".to_string(),
                Err(e) => format!("metrics server error: {e}"),
            }))
        }
    });

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: BACKOFF_FIRST_MS,
        max_ms: BACKOFF_MAX_MS,
        factor: BACKOFF_FACTOR,
    };
    let embedded = EmbeddedSpec::new(revision)?;
    let spec = TaskSpec::builder(
        METRICS_SERVER_SLOT,
        TaskWorkload::Embedded(embedded),
        METRICS_SERVER_TIMEOUT_MS,
    )
    .restart(RestartPolicy::always())
    .backoff(backoff)
    .admission(AdmissionPolicy::Replace)
    .build()?;

    let manifest = TaskManifest::new(METRICS_SERVER_SLOT, spec)?;

    Ok((manifest, task))
}

struct MetricsServerState {
    registry: Arc<Registry>,
    gather_slots: Arc<Semaphore>,
    max_response_bytes: usize,
    scrape_timeout: Duration,
}

fn metrics_router(state: Arc<MetricsServerState>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler).head(reject_metrics_head))
        .with_state(state)
}

async fn reject_metrics_head() -> axum::response::Response {
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET"));
    response
}

impl MetricsServerState {
    fn new(registry: Arc<Registry>, config: MetricsServerConfig) -> Self {
        Self {
            registry,
            gather_slots: Arc::new(Semaphore::new(config.max_concurrent_scrapes().get())),
            max_response_bytes: config.max_response_bytes().get(),
            scrape_timeout: config.scrape_timeout(),
        }
    }
}

async fn metrics_handler(State(state): State<Arc<MetricsServerState>>) -> axum::response::Response {
    let permit = match Arc::clone(&state.gather_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            report_without_unwind(|| {
                debug!(
                    event = "metrics.scrape_rejected",
                    error_kind = "saturated",
                    "metrics scrape rejected because every ownership slot is occupied"
                );
            });
            let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return response;
        }
    };

    let registry = Arc::clone(&state.registry);
    let max_response_bytes = state.max_response_bytes;
    let job = tokio::task::spawn_blocking(move || {
        render_scrape(registry.as_ref(), max_response_bytes, permit)
    });

    match tokio::time::timeout(state.scrape_timeout, job).await {
        Ok(Ok(Ok(completed))) => completed_scrape_response(completed),
        Ok(Ok(Err(ScrapeFailure::ResponseTooLarge))) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Ok(Ok(Err(ScrapeFailure::Encode))) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Ok(Ok(Err(ScrapeFailure::Panicked))) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Ok(Err(error)) => {
            let error_kind = if error.is_panic() {
                "worker_panicked"
            } else {
                "worker_cancelled"
            };
            let error_message = error.to_string();
            if error.is_panic() {
                dispose_panic_payload(error.into_panic());
            }
            report_without_unwind(|| {
                error!(
                    event = "metrics.scrape_worker_failed",
                    error_kind,
                    error = %error_message,
                    "metrics scrape blocking worker failed"
                );
            });
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => {
            report_without_unwind(|| {
                warn!(
                    event = "metrics.scrape_timed_out",
                    error_kind = "timeout",
                    timeout_ns = state.scrape_timeout.as_nanos(),
                    "metrics scrape response deadline elapsed; physical gather retains its slot"
                );
            });
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrapeFailure {
    ResponseTooLarge,
    Encode,
    Panicked,
}

struct CompletedScrape {
    bytes: Vec<u8>,
    // Declared last so detached job output frees bytes before re-admission.
    permit: OwnedSemaphorePermit,
}

fn render_scrape(
    registry: &Registry,
    max_response_bytes: usize,
    permit: OwnedSemaphorePermit,
) -> Result<CompletedScrape, ScrapeFailure> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        encode_scrape(registry, max_response_bytes)
    }));
    match result {
        Ok(Ok(bytes)) => {
            report_without_unwind(|| {
                debug!(
                    event = "metrics.scrape_completed",
                    outcome = "success",
                    response_bytes = bytes.len(),
                    "metrics gather and encoding completed"
                );
            });
            Ok(CompletedScrape { bytes, permit })
        }
        Ok(Err(error)) => Err(error),
        Err(payload) => {
            dispose_panic_payload(payload);
            report_without_unwind(|| {
                error!(
                    event = "metrics.scrape_failed",
                    error_kind = "scrape_panicked",
                    "metrics gather or encoding panicked"
                );
            });
            Err(ScrapeFailure::Panicked)
        }
    }
}

fn completed_scrape_response(completed: CompletedScrape) -> axum::response::Response {
    let response_bytes = completed.bytes.len();
    let bytes = Bytes::from_owner(ScrapePayload {
        bytes: completed.bytes,
        _permit: completed.permit,
    });
    let body = Body::from_stream(SingleChunkBodyStream { bytes: Some(bytes) });
    let mut response = axum::response::Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(METRICS_CONTENT_TYPE),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&response_bytes.to_string())
            .expect("a decimal usize must be a valid Content-Length header"),
    );
    response
}

struct ScrapePayload {
    bytes: Vec<u8>,
    // Declared last so the payload memory drops before the slot is released.
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for ScrapePayload {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

struct SingleChunkBodyStream {
    bytes: Option<Bytes>,
}

impl Stream for SingleChunkBodyStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.bytes.take().map(Ok))
    }
}

fn encode_scrape(registry: &Registry, max_response_bytes: usize) -> Result<Vec<u8>, ScrapeFailure> {
    let encoder = TextEncoder::new();
    let families = registry.gather();
    let mut writer = BoundedWriter::new(max_response_bytes);
    match encoder.encode(&families, &mut writer) {
        Ok(()) => Ok(writer.into_inner()),
        Err(_) if writer.limit_exceeded() => {
            report_without_unwind(|| {
                error!(
                    event = "metrics.scrape_failed",
                    error_kind = "response_too_large",
                    max_response_bytes,
                    "metrics exposition exceeds the response byte limit"
                );
            });
            Err(ScrapeFailure::ResponseTooLarge)
        }
        Err(error) => {
            report_without_unwind(|| {
                error!(
                    event = "metrics.scrape_failed",
                    error_kind = "encode_failed",
                    %error,
                    "metrics encoding failed"
                );
            });
            Err(ScrapeFailure::Encode)
        }
    }
}

fn dispose_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    // A custom payload may panic again from Drop. Forget the replacement to
    // contain that second unwind; the public server contract documents this
    // non-sticky leak boundary.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(payload);
    }
}

fn report_without_unwind(report: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(report)) {
        dispose_panic_payload(payload);
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    limit_exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            limit_exceeded: false,
        }
    }

    fn limit_exceeded(&self) -> bool {
        self.limit_exceeded
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("metrics response byte limit exceeded"));
        };
        if next_len > self.limit {
            self.limit_exceeded = true;
            return Err(io::Error::other("metrics response byte limit exceeded"));
        }
        if self.bytes.capacity().saturating_sub(self.bytes.len()) < bytes.len() {
            let grown_capacity = if self.bytes.capacity() == 0 {
                INITIAL_RESPONSE_CAPACITY
            } else {
                self.bytes.capacity().saturating_mul(2)
            };
            let target_capacity = grown_capacity.min(self.limit).max(next_len);
            self.bytes
                .try_reserve_exact(target_capacity - self.bytes.len())
                .map_err(|_| io::Error::other("metrics response allocation failed"))?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod ownership_tests;

#[cfg(test)]
mod tests {
    use std::{
        env,
        process::Command,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        thread::ThreadId,
    };

    use axum::{
        body::{Body, HttpBody, to_bytes},
        http::{Method, Request},
    };
    use prometheus::{
        Counter,
        core::{Collector, Desc},
        proto::MetricFamily,
    };
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn config_defaults_are_bounded() {
        let config = MetricsServerConfig::default();

        assert_eq!(config.max_concurrent_scrapes().get(), 2);
        assert_eq!(config.max_response_bytes().get(), 4 * 1024 * 1024);
        assert_eq!(config.scrape_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn config_accepts_ceilings_and_rejects_values_outside_them() {
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(MetricsServerConfig::MAX_CONCURRENT_SCRAPES.get())
            .unwrap()
            .try_with_max_response_bytes(MetricsServerConfig::MAX_RESPONSE_BYTES.get())
            .unwrap()
            .try_with_scrape_timeout(MetricsServerConfig::MAX_SCRAPE_TIMEOUT)
            .unwrap();

        assert_eq!(
            config.max_concurrent_scrapes(),
            MetricsServerConfig::MAX_CONCURRENT_SCRAPES
        );
        assert_eq!(
            config.max_response_bytes(),
            MetricsServerConfig::MAX_RESPONSE_BYTES
        );
        assert_eq!(
            config.scrape_timeout(),
            MetricsServerConfig::MAX_SCRAPE_TIMEOUT
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_max_concurrent_scrapes(0)
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_max_concurrent_scrapes(
                    MetricsServerConfig::MAX_CONCURRENT_SCRAPES.get() + 1,
                )
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_max_response_bytes(0)
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_max_response_bytes(MetricsServerConfig::MAX_RESPONSE_BYTES.get() + 1)
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_scrape_timeout(Duration::ZERO)
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_scrape_timeout(Duration::from_micros(999))
                .is_err()
        );
        assert!(
            MetricsServerConfig::new()
                .try_with_scrape_timeout(
                    MetricsServerConfig::MAX_SCRAPE_TIMEOUT + Duration::from_nanos(1)
                )
                .is_err()
        );
    }

    #[test]
    fn server_revision_covers_caller_state_address_and_default_scrape_policy() {
        let (manifest, _) = server(
            Arc::new(Registry::new()),
            "127.0.0.1:9090",
            "agent-registry-v2",
        )
        .unwrap();
        let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
            panic!("metrics server must use an Embedded workload");
        };

        assert_eq!(
            embedded.revision(),
            "agent-registry-v2|addr=127.0.0.1:9090|max_concurrent_scrapes=2|max_response_bytes=4194304|scrape_timeout_ns=10000000000"
        );
    }

    #[test]
    fn configured_server_revision_covers_every_effective_scrape_setting() {
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(3)
            .unwrap()
            .try_with_max_response_bytes(12_345)
            .unwrap()
            .try_with_scrape_timeout(Duration::from_millis(678))
            .unwrap();
        let (manifest, _) = server_with_config(
            Arc::new(Registry::new()),
            "127.0.0.1:9191",
            "agent-registry-v3",
            config,
        )
        .unwrap();
        let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
            panic!("metrics server must use an Embedded workload");
        };

        assert_eq!(
            embedded.revision(),
            "agent-registry-v3|addr=127.0.0.1:9191|max_concurrent_scrapes=3|max_response_bytes=12345|scrape_timeout_ns=678000000"
        );
    }

    #[test]
    fn server_revision_preserves_sub_millisecond_timeout_precision() {
        let timeout = Duration::from_millis(1) + Duration::from_nanos(1);
        let config = MetricsServerConfig::new()
            .try_with_scrape_timeout(timeout)
            .unwrap();
        let (manifest, _) = server_with_config(
            Arc::new(Registry::new()),
            "127.0.0.1:9191",
            "precision",
            config,
        )
        .unwrap();
        let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
            panic!("metrics server must use an Embedded workload");
        };

        assert!(embedded.revision().ends_with("scrape_timeout_ns=1000001"));
    }

    #[test]
    fn server_rejects_an_empty_caller_revision() {
        assert!(server(Arc::new(Registry::new()), "127.0.0.1:9090", "  ").is_err());
    }

    #[test]
    fn bounded_writer_accepts_exact_limit_and_rejects_plus_one_atomically() {
        let mut exact = BoundedWriter::new(4);
        exact.write_all(b"four").unwrap();
        assert_eq!(exact.into_inner(), b"four");

        let mut plus_one = BoundedWriter::new(4);
        assert!(plus_one.write_all(b"12345").is_err());
        assert!(plus_one.limit_exceeded());
        assert!(plus_one.into_inner().is_empty());
    }

    #[test]
    fn encoded_exposition_accepts_exact_limit_and_rejects_one_byte_less() {
        let registry = Registry::new();
        let counter = Counter::new("exact_limit_total", "exact response limit").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();

        let bytes =
            encode_scrape(&registry, MetricsServerConfig::MAX_RESPONSE_BYTES.get()).unwrap();
        assert_eq!(
            encode_scrape(&registry, bytes.len()).unwrap(),
            bytes,
            "the exact encoded size must be accepted"
        );
        assert_eq!(
            encode_scrape(&registry, bytes.len() - 1),
            Err(ScrapeFailure::ResponseTooLarge)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_limit_failure_sends_no_partial_exposition() {
        let registry = Arc::new(Registry::new());
        let counter = Counter::new("oversized_total", "oversized response").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_response_bytes(1)
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let response = metrics_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        assert!(response_body(response).await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gather_and_encoding_run_off_the_tokio_worker() {
        let registry = Arc::new(Registry::new());
        let (collector, mut control) = blocking_collector("blocking_thread_total");
        registry.register(Box::new(collector)).unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));
        let tokio_worker = std::thread::current().id();

        let request = tokio::spawn(metrics_handler(State(state)));
        let collector_thread = control.wait_until_entered().await;
        control.release();
        let response = request.await.unwrap();

        assert_ne!(collector_thread, tokio_worker);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_scrape_is_rejected_without_waiting() {
        let registry = Arc::new(Registry::new());
        let (collector, mut control) = blocking_collector("saturation_total");
        registry.register(Box::new(collector)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(1)
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let first = tokio::spawn(metrics_handler(State(Arc::clone(&state))));
        control.wait_until_entered().await;
        let rejected = metrics_handler(State(Arc::clone(&state))).await;

        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejected.headers()[header::RETRY_AFTER], "1");
        assert!(response_body(rejected).await.is_empty());

        control.release();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_gather_keeps_its_slot_until_physical_return() {
        let registry = Arc::new(Registry::new());
        let (collector, mut control) = blocking_collector("timeout_slot_total");
        registry.register(Box::new(collector)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(1)
            .unwrap()
            .try_with_scrape_timeout(Duration::from_secs(10))
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let first = tokio::spawn(metrics_handler(State(Arc::clone(&state))));
        control.wait_until_entered().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(first.await.unwrap().status(), StatusCode::GATEWAY_TIMEOUT);

        let while_still_blocked = metrics_handler(State(Arc::clone(&state))).await;
        assert_eq!(
            while_still_blocked.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        // The next wait observes a real blocking thread, not virtual time.
        tokio::time::resume();
        control.release();
        wait_for_available_slots(&state, 1).await;
        assert_eq!(metrics_handler(State(state)).await.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_request_keeps_its_slot_until_physical_return() {
        let registry = Arc::new(Registry::new());
        let (collector, mut control) = blocking_collector("cancelled_slot_total");
        registry.register(Box::new(collector)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(1)
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let first = tokio::spawn(metrics_handler(State(Arc::clone(&state))));
        control.wait_until_entered().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let while_still_blocked = metrics_handler(State(Arc::clone(&state))).await;
        assert_eq!(
            while_still_blocked.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        control.release();
        wait_for_available_slots(&state, 1).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_requests_release_every_slot_without_followup_requests() {
        for cycle in 0..128 {
            let registry = Arc::new(Registry::new());
            let (collector, mut control) = blocking_collector("cancelled_release_total");
            registry.register(Box::new(collector)).unwrap();
            let state = Arc::new(MetricsServerState::new(
                registry,
                MetricsServerConfig::new()
                    .try_with_max_concurrent_scrapes(1)
                    .unwrap(),
            ));
            let request = tokio::spawn(metrics_handler(State(Arc::clone(&state))));
            tokio::time::timeout(Duration::from_secs(5), control.wait_until_entered())
                .await
                .expect("physical collector did not enter");
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            assert_eq!(state.gather_slots.available_permits(), 0);
            control.release();
            // No new handler call or HTTP request drives this release.
            wait_for_available_slots(&state, 1).await;
            assert_eq!(state.gather_slots.available_permits(), 1, "cycle {cycle}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collector_panic_is_isolated_as_an_internal_server_error() {
        let registry = Arc::new(Registry::new());
        registry
            .register(Box::new(PanickingCollector {
                counter: Counter::new("panicking_total", "panicking collector").unwrap(),
            }))
            .unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));

        let response = metrics_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response_body(response).await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn head_is_rejected_without_gathering() {
        let registry = Arc::new(Registry::new());
        let (collector, mut control) = blocking_collector("head_must_not_gather_total");
        registry.register(Box::new(collector)).unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));
        let response = metrics_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET");
        assert!(control.entered.as_mut().unwrap().try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn destructor_panicking_payload_is_contained() {
        let registry = Arc::new(Registry::new());
        registry
            .register(Box::new(DestructorPanickingCollector {
                counter: Counter::new(
                    "destructor_panicking_total",
                    "destructor-panicking collector",
                )
                .unwrap(),
            }))
            .unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));

        let response = metrics_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response_body(response).await.is_empty());
    }

    #[test]
    fn hostile_collector_and_tracing_panics_are_contained_in_subprocess() {
        const CHILD_ENV: &str = "SOLTI_PROMETHEUS_HOSTILE_REPORT_CHILD";

        if env::var_os(CHILD_ENV).is_some() {
            run_hostile_report_child();
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
            .expect("the hostile-report child test must start");

        assert!(
            output.status.success(),
            "hostile-report child failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn run_hostile_report_child() {
        let reports = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::set_global_default(HostileSubscriber {
            reports: Arc::clone(&reports),
        })
        .expect("the isolated child must install its tracing subscriber once");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let successful_registry = Arc::new(Registry::new());
        let counter = Counter::new("hostile_report_success_total", "successful scrape").unwrap();
        counter.inc();
        successful_registry.register(Box::new(counter)).unwrap();
        let successful_state = Arc::new(MetricsServerState::new(
            successful_registry,
            MetricsServerConfig::default(),
        ));
        let successful = runtime.block_on(metrics_handler(State(successful_state)));
        assert_eq!(successful.status(), StatusCode::OK);

        let panicking_registry = Arc::new(Registry::new());
        panicking_registry
            .register(Box::new(DestructorPanickingCollector {
                counter: Counter::new("hostile_report_panic_total", "panicking scrape").unwrap(),
            }))
            .unwrap();
        let panicking_state = Arc::new(MetricsServerState::new(
            panicking_registry,
            MetricsServerConfig::default(),
        ));
        let panicking = runtime.block_on(metrics_handler(State(panicking_state)));
        assert_eq!(panicking.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            reports.load(Ordering::SeqCst) >= 2,
            "both success and panic reports must reach the hostile subscriber"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn encoding_failure_is_isolated_as_an_internal_server_error() {
        let registry = Arc::new(Registry::new());
        registry
            .register(Box::new(InvalidCollector {
                counter: Counter::new("invalid_family_total", "invalid family").unwrap(),
            }))
            .unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));

        let response = metrics_handler(State(state)).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response_body(response).await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_response_owns_its_body_after_server_state_drops() {
        let registry = Arc::new(Registry::new());
        let counter = Counter::new("owned_response_total", "owned response").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();
        let state = Arc::new(MetricsServerState::new(
            registry,
            MetricsServerConfig::default(),
        ));

        let response = metrics_handler(State(Arc::clone(&state))).await;
        let content_length: usize = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        drop(state);
        let body = response_body(response).await;

        assert_eq!(content_length, body.len());
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("owned_response_total")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_response_holds_slot_until_response_body_drops() {
        let registry = Arc::new(Registry::new());
        let counter = Counter::new("response_slot_total", "response slot").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(1)
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let completed = metrics_handler(State(Arc::clone(&state))).await;
        assert_eq!(completed.status(), StatusCode::OK);
        assert_eq!(state.gather_slots.available_permits(), 0);

        let while_body_is_owned = metrics_handler(State(Arc::clone(&state))).await;
        assert_eq!(
            while_body_is_owned.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        drop(completed);
        wait_for_available_slots(&state, 1).await;
        assert_eq!(metrics_handler(State(state)).await.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn yielded_response_bytes_clones_keep_slot_after_body_drops() {
        let registry = Arc::new(Registry::new());
        let counter = Counter::new("yielded_bytes_slot_total", "yielded bytes slot").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();
        let config = MetricsServerConfig::new()
            .try_with_max_concurrent_scrapes(1)
            .unwrap();
        let state = Arc::new(MetricsServerState::new(registry, config));

        let response = metrics_handler(State(Arc::clone(&state))).await;
        let mut body = response.into_body();
        let frame = std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context))
            .await
            .expect("successful response must contain one frame")
            .expect("successful response frame must be infallible");
        let bytes = match frame.into_data() {
            Ok(bytes) => bytes,
            Err(_) => panic!("successful response frame must contain data"),
        };
        let transport_clone = bytes.clone();
        drop(body);
        drop(bytes);

        assert_eq!(state.gather_slots.available_permits(), 0);
        assert_eq!(
            metrics_handler(State(Arc::clone(&state))).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        drop(transport_clone);
        wait_for_available_slots(&state, 1).await;
        assert_eq!(metrics_handler(State(state)).await.status(), StatusCode::OK);
    }

    async fn response_body(response: axum::response::Response) -> Vec<u8> {
        to_bytes(
            response.into_body(),
            MetricsServerConfig::MAX_RESPONSE_BYTES.get(),
        )
        .await
        .unwrap()
        .to_vec()
    }

    async fn wait_for_available_slots(state: &MetricsServerState, expected: usize) {
        // A fixed yield count is not a blocking-pool completion barrier.
        // Acquire the actual released slots without another handler/HTTP call.
        let permit = tokio::time::timeout(
            Duration::from_secs(5),
            state
                .gather_slots
                .acquire_many(expected.try_into().unwrap()),
        )
        .await
        .expect("physical gather output did not release its semaphore permit")
        .expect("gather semaphore unexpectedly closed");
        drop(permit);
    }

    struct BlockingCollector {
        counter: Counter,
        entered: Mutex<Option<oneshot::Sender<ThreadId>>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct BlockingCollectorControl {
        entered: Option<oneshot::Receiver<ThreadId>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl BlockingCollectorControl {
        async fn wait_until_entered(&mut self) -> ThreadId {
            self.entered
                .take()
                .expect("collector entry receiver must be used once")
                .await
                .expect("collector must report its blocking thread")
        }

        fn release(&self) {
            let (released, wake) = &*self.release;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
    }

    impl Drop for BlockingCollectorControl {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn blocking_collector(name: &str) -> (BlockingCollector, BlockingCollectorControl) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        (
            BlockingCollector {
                counter: Counter::new(name, "blocking collector").unwrap(),
                entered: Mutex::new(Some(entered_tx)),
                release: Arc::clone(&release),
            },
            BlockingCollectorControl {
                entered: Some(entered_rx),
                release,
            },
        )
    }

    impl Collector for BlockingCollector {
        fn desc(&self) -> Vec<&Desc> {
            self.counter.desc()
        }

        fn collect(&self) -> Vec<MetricFamily> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(std::thread::current().id());
            }
            let (released, wake) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            drop(released);
            self.counter.collect()
        }
    }

    struct PanickingCollector {
        counter: Counter,
    }

    impl Collector for PanickingCollector {
        fn desc(&self) -> Vec<&Desc> {
            self.counter.desc()
        }

        fn collect(&self) -> Vec<MetricFamily> {
            panic!("collector panic")
        }
    }

    struct InvalidCollector {
        counter: Counter,
    }

    struct DestructorPanickingCollector {
        counter: Counter,
    }

    struct DestructorPanickingPayload;

    struct HostileSubscriber {
        reports: Arc<AtomicUsize>,
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
            std::panic::panic_any(DestructorPanickingPayload);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    impl Drop for DestructorPanickingPayload {
        fn drop(&mut self) {
            panic!("panic payload destructor")
        }
    }

    impl Collector for DestructorPanickingCollector {
        fn desc(&self) -> Vec<&Desc> {
            self.counter.desc()
        }

        fn collect(&self) -> Vec<MetricFamily> {
            std::panic::panic_any(DestructorPanickingPayload)
        }
    }

    impl Collector for InvalidCollector {
        fn desc(&self) -> Vec<&Desc> {
            self.counter.desc()
        }

        fn collect(&self) -> Vec<MetricFamily> {
            let mut families = self.counter.collect();
            families[0].clear_name();
            families
        }
    }
}
