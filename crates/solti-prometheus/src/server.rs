//! # Metrics server
//!
//! [`server`] builds an embedded task for a Prometheus endpoint.
//! The task reads one shared [`Registry`].
//!
//! Enable it with the `server` feature.
//!
//! ## Flow
//!
//! ```text
//! Registry + address + revision
//!              ▼
//!          server()
//!              ├──► TaskManifest
//!              └──► TaskRef ──► bind address ──► GET /metrics
//! ```
//!
//! `server()` does not bind the address.
//! Binding starts when Taskvisor runs the returned task.

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
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy, TaskManifest,
    TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tracing::{debug, error, info};

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

/// Initial backoff delay on failure in milliseconds.
const BACKOFF_FIRST_MS: u64 = 1_000;

/// Maximum backoff delay on repeated failures in milliseconds.
const BACKOFF_MAX_MS: u64 = 30_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

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
///
/// The composed embedded revision contains the caller revision and listen address.
/// Changing either value changes the revision.
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
    let addr: String = addr.into();
    let caller_revision = EmbeddedSpec::new(revision)?;
    let revision = format!("{}|addr={addr}", caller_revision.revision());

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
            info!(
                event = "metrics.server_started",
                listen_addr = %addr,
                path = "/metrics",
                "metrics server started"
            );

            let shutdown_ctx = ctx.clone();
            let serve_result = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_ctx.cancelled().await })
                .await;

            if ctx.is_cancelled() {
                debug!(
                    event = "metrics.server_stopped",
                    reason = "canceled",
                    "metrics server stopped"
                );
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

async fn metrics_handler(State(registry): State<Arc<Registry>>) -> axum::response::Response {
    let encoder = TextEncoder::new();
    let mut buf = Vec::with_capacity(4096);
    match encoder.encode(&registry.gather(), &mut buf) {
        Ok(()) => ([(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)], buf).into_response(),
        Err(error) => {
            error!(
                event = "metrics.encode_failed",
                %error,
                "metrics encoding failed"
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_revision_covers_caller_state_and_listen_address() {
        let (manifest, _) = server(
            Arc::new(Registry::new()),
            "127.0.0.1:9090",
            "agent-registry-v2",
        )
        .unwrap();
        let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
            panic!("metrics server must use an Embedded workload");
        };

        assert_eq!(embedded.revision(), "agent-registry-v2|addr=127.0.0.1:9090");
    }

    #[test]
    fn server_rejects_an_empty_caller_revision() {
        assert!(server(Arc::new(Registry::new()), "127.0.0.1:9090", "  ").is_err());
    }
}
