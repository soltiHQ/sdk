//! # Embedded metrics HTTP server (feature `server`).
//!
//! [`server`] builds a supervised axum task that serves `/metrics` from a shared [`Registry`].
//! Bind and serve failures are retried under supervisor backoff.
//!
//! Shutdown is cooperative through [`TaskContext`](taskvisor::TaskContext).
//! The task runs under [`AdmissionPolicy::Replace`] in the [`METRICS_SERVER_SLOT`] slot.
//!
//! See the [crate root](crate) for the namespace/architecture overview.

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use prometheus::{Encoder, Registry, TextEncoder};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tracing::{debug, error, info};

/// Logical slot name for the metrics server task.
///
/// ## Example
///
/// ```
/// assert_eq!(solti_prometheus::METRICS_SERVER_SLOT, "solti-metrics-server");
/// ```
pub const METRICS_SERVER_SLOT: &str = "solti-metrics-server";

/// Per-attempt timeout in milliseconds (effectively infinite for this long-running server).
const METRICS_SERVER_TIMEOUT_MS: u64 = u64::MAX;

/// Prometheus text exposition content-type (format version 0.0.4).
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Initial backoff delay on failure in milliseconds.
const BACKOFF_FIRST_MS: u64 = 1_000;

/// Maximum backoff delay on repeated failures in milliseconds.
const BACKOFF_MAX_MS: u64 = 30_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Build the metrics HTTP server task and its supervision spec.
///
/// Serves `/metrics` (Prometheus text exposition format) from the given shared [`Registry`].
/// The task runs under supervisor control so bind and serve errors are retried with backoff;
/// graceful shutdown is propagated via the task's [`TaskContext`](taskvisor::TaskContext).
///
/// ## Scheduling
///
/// | Scenario      | Delay           | Strategy                              |
/// |---------------|-----------------|---------------------------------------|
/// | Success       | Immediate       | Always restart (server runs forever)  |
/// | Failure       | 1 s to 30 s     | Exponential backoff with equal jitter |
/// | Duplicate     | Replaces        | [`AdmissionPolicy::Replace`]          |
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
/// let (task, spec) = server(registry.clone(), "0.0.0.0:9090");
/// // Submit to a running supervisor:
/// // supervisor.submit_with_task(task, &spec).await?;
/// # let _ = (task, spec);
/// ```
pub fn server(registry: Arc<Registry>, addr: impl Into<String>) -> (TaskRef, TaskSpec) {
    let addr: String = addr.into();

    let task: TaskRef = TaskFn::arc(METRICS_SERVER_SLOT, move |ctx: TaskContext| {
        let addr = addr.clone();
        let registry = registry.clone();
        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }

            let app = Router::new()
                .route("/metrics", get(metrics_handler))
                .with_state(registry);

            let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
                TaskError::fail(format!("metrics listener bind failed on {addr}: {e}"))
            })?;
            debug!(addr = %addr, "metrics server started");
            info!("metrics http://{addr}/metrics");

            let shutdown_ctx = ctx.clone();
            let serve_result = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_ctx.cancelled().await })
                .await;

            if ctx.is_cancelled() {
                debug!("metrics server canceled");
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
    let spec = TaskSpec::builder(
        METRICS_SERVER_SLOT,
        TaskKind::Embedded,
        METRICS_SERVER_TIMEOUT_MS,
    )
    .restart(RestartPolicy::always())
    .backoff(backoff)
    .admission(AdmissionPolicy::Replace)
    .build()
    .expect("metrics server spec must be valid");

    (task, spec)
}

async fn metrics_handler(State(registry): State<Arc<Registry>>) -> axum::response::Response {
    let encoder = TextEncoder::new();
    let mut buf = Vec::with_capacity(4096);
    match encoder.encode(&registry.gather(), &mut buf) {
        Ok(()) => ([(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)], buf).into_response(),
        Err(e) => {
            error!("metrics encode error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
