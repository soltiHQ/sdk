//! # Periodic sync (heartbeat) task.
//!
//! ```text
//! Agent                          Control Plane
//!   |                                  |
//!   |--- SyncRequest (gRPC / HTTP) --->|
//!   |<-- SyncResponse (success) -------|
//!   |         ... delay_ms ...         |
//!   |--- SyncRequest ----------------> |
//! ```
//!
//! On failure the task returns `TaskError::Fail` and the supervisor applies backoff + restart policy from the spec.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(any(feature = "grpc", feature = "http"))]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bound on server-advised hold time (seconds).
const MAX_RETRY_AFTER_S: i32 = 3_600;

use crate::proto::SyncRequest;

use std::time::Instant;

use tracing::{debug, warn};

use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy, TaskManifest,
    TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

use crate::config::DiscoverConfig;
use crate::errors::DiscoverError;
use crate::metrics::{self, DiscoverMetricsHandle};
use crate::tasks::transport::TransportAdapter;
use crate::uptime::UptimeSource;

const SLOT: &str = "solti-discover-sync";

/// Build a heartbeat task and its spec from discovery config and an uptime source.
///
/// Returns a prebuilt runtime task and its complete desired resource.
///
/// ## Errors
///
/// - [`DiscoverError::SpecBuild`]: `TaskSpec` validation failed.
/// - [`DiscoverError::HttpRequest`]: the `reqwest::Client` builder failed (feature `http`; rare - e.g. TLS backend init).
/// - [`DiscoverError::InvalidConfig`]: the TLS config could not be converted into a `rustls` client config (features `http` + `tls`).
///
/// ## Also
///
/// - [`DiscoverConfig`](crate::DiscoverConfig) controls endpoint, transport, interval.
/// - [`DiscoverError`](crate::DiscoverError) failure modes surfaced via `TaskError::Fail`.
/// - [`UptimeSource`](crate::UptimeSource) defines the composition-owned uptime epoch.
pub fn sync(
    config: DiscoverConfig,
    uptime: Arc<dyn UptimeSource>,
) -> Result<(TaskRef, TaskManifest), DiscoverError> {
    let delay_ms = config.delay_ms;

    let backoff = config.backoff.clone().unwrap_or_else(|| BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: (delay_ms / 2).max(1),
        max_ms: delay_ms.saturating_mul(3),
        factor: 2.0,
    });

    // The per-attempt timeout must cover everything the attempt body may
    // legitimately wait through: one-time startup jitter (up to delay_ms),
    // a server-advised retry hold (up to MAX_RETRY_AFTER_S), and the request
    // itself. Tying it to the heartbeat period would kill healthy attempts.
    let attempt_timeout_ms = delay_ms
        .saturating_add((MAX_RETRY_AFTER_S as u64).saturating_mul(1_000))
        .saturating_add(config.connect_timeout_ms)
        .saturating_add(config.request_timeout_ms)
        .saturating_add(1_000);

    let embedded = EmbeddedSpec::new(config.task_revision.clone())
        .map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;
    let spec = TaskSpec::builder(SLOT, TaskWorkload::Embedded(embedded), attempt_timeout_ms)
        .restart(RestartPolicy::periodic(delay_ms))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;

    let base_request = build_base_request(&config);

    if config.token.is_some() && !tls_enabled(&config) {
        warn!(
            "discovery: presenting a bearer token over a plaintext channel; \
             enable TLS to protect the credential in transit"
        );
    }

    let transport = TransportAdapter::from_config(&config)?;
    let metrics = config.metrics.clone();

    let ctx = Arc::new(SyncContext {
        base_request,
        delay_ms,
        transport,
        retry_hold_until: AtomicU64::new(0),
        startup_jitter_applied: AtomicBool::new(false),
        metrics,
        uptime,
    });

    let task: TaskRef = TaskFn::arc(SLOT, move |cancel: TaskContext| {
        let ctx = Arc::clone(&ctx);

        async move {
            if !ctx.startup_jitter_applied.swap(true, Ordering::Relaxed) {
                let jitter = Duration::from_millis(startup_jitter_ms(ctx.delay_ms));
                debug!(
                    jitter_ms = jitter.as_millis() as u64,
                    "applying startup jitter before first sync",
                );
                cancel
                    .run_until_cancelled(tokio::time::sleep(jitter))
                    .await?;
            }

            if let Some(wait) = compute_hold_wait(
                ctx.retry_hold_until.load(Ordering::Relaxed),
                now_unix_seconds(),
            ) {
                debug!(
                    wait_s = wait.as_secs(),
                    "waiting for server-advised retry hold"
                );
                cancel.run_until_cancelled(tokio::time::sleep(wait)).await?;
            }

            debug!("sending sync request to control plane");
            ctx.metrics.record_attempt();
            let start = Instant::now();
            let request = stamp_request(&ctx.base_request, ctx.uptime.as_ref());
            let result = cancel
                .run_until_cancelled(ctx.transport.sync(request))
                .await?;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(()) => {
                    ctx.metrics.record_success(duration_ms);
                    ctx.retry_hold_until.store(0, Ordering::Relaxed);
                    debug!("sync completed successfully");
                    Ok(())
                }
                Err(e) => {
                    ctx.metrics
                        .record_failure(duration_ms, classify_failure(&e));
                    if let DiscoverError::Rejected {
                        retry_after_s: Some(s),
                        ..
                    } = &e
                    {
                        let clamped = (*s).clamp(0, MAX_RETRY_AFTER_S);
                        if *s != clamped {
                            warn!(advised_s = *s, capped_s = clamped, "retry_after_s capped",);
                        }
                        let hold_until = now_unix_seconds().saturating_add(clamped as u64);
                        ctx.retry_hold_until.store(hold_until, Ordering::Relaxed);
                        ctx.metrics.record_hold(clamped as u64);
                    }

                    if e.is_terminal() {
                        warn!("sync failed fatally: {}", e);
                        Err(TaskError::fatal(format!("sync fatally failed: {}", e)))
                    } else {
                        warn!("sync failed: {}", e);
                        Err(TaskError::fail(format!("sync failed: {}", e)))
                    }
                }
            }
        }
    });
    let manifest =
        TaskManifest::new(SLOT, spec).map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;
    Ok((task, manifest))
}

struct SyncContext {
    base_request: SyncRequest,
    delay_ms: u64,
    transport: TransportAdapter,
    /// Unix timestamp (seconds) before which the next sync attempt must wait.
    ///
    /// `0` means no active hold. Updated to `now + retry_after_s` when the control plane returns `Rejected { retry_after_s: Some(_) }`;
    /// cleared to `0` on successful sync.
    retry_hold_until: AtomicU64,
    /// Guard that makes the first-tick startup jitter run exactly once per process lifetime.
    /// Set to `true` after the initial jitter sleep;
    /// subsequent restarts (e.g. after a transient failure) skip the jitter and stay on the periodic schedule.
    startup_jitter_applied: AtomicBool,
    metrics: DiscoverMetricsHandle,
    uptime: Arc<dyn UptimeSource>,
}

/// Map a [`DiscoverError`] to a canonical failure-reason label.
fn classify_failure(err: &DiscoverError) -> metrics::DiscoverFailReason {
    match err {
        DiscoverError::InvalidConfig(_) | DiscoverError::SpecBuild(_) => {
            metrics::DiscoverFailReason::Other
        }
        DiscoverError::Rejected { .. } => metrics::DiscoverFailReason::RejectedClient,
        DiscoverError::AuthFailed { .. } => metrics::DiscoverFailReason::Auth,
        #[cfg(feature = "http")]
        DiscoverError::HttpRequest(e) => {
            if e.is_timeout() {
                metrics::DiscoverFailReason::Timeout
            } else if e.is_connect() {
                metrics::DiscoverFailReason::Connect
            } else if e.is_decode() || e.is_body() {
                metrics::DiscoverFailReason::Parse
            } else {
                metrics::DiscoverFailReason::Other
            }
        }
        #[cfg(feature = "http")]
        DiscoverError::HttpStatus { code, .. } => {
            if *code >= 500 {
                metrics::DiscoverFailReason::RejectedServer
            } else {
                metrics::DiscoverFailReason::RejectedClient
            }
        }
        #[cfg(feature = "http")]
        DiscoverError::InvalidResponse(_) => metrics::DiscoverFailReason::Parse,
        #[cfg(feature = "grpc")]
        DiscoverError::GrpcTransport(_) => metrics::DiscoverFailReason::Connect,
        #[cfg(feature = "grpc")]
        DiscoverError::GrpcStatus(s) => {
            use tonic::Code;
            match s.code() {
                Code::DeadlineExceeded => metrics::DiscoverFailReason::Timeout,
                Code::Unavailable | Code::Internal | Code::DataLoss => {
                    metrics::DiscoverFailReason::RejectedServer
                }
                Code::Unauthenticated => metrics::DiscoverFailReason::Auth,
                Code::PermissionDenied
                | Code::InvalidArgument
                | Code::FailedPrecondition
                | Code::NotFound
                | Code::AlreadyExists
                | Code::OutOfRange
                | Code::Aborted
                | Code::Cancelled => metrics::DiscoverFailReason::RejectedClient,
                _ => metrics::DiscoverFailReason::Other,
            }
        }
    }
}

/// Whether outbound TLS is configured.
#[inline]
fn tls_enabled(_config: &DiscoverConfig) -> bool {
    #[cfg(feature = "tls")]
    {
        _config.tls.is_some()
    }
    #[cfg(not(feature = "tls"))]
    {
        false
    }
}

#[inline]
fn platform() -> &'static str {
    std::env::consts::OS
}

#[inline]
fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Get OS distribution info (Linux only).
///
/// Checks `/etc/os-release`, then `/usr/lib/os-release` (freedesktop spec fallback).
fn os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        for path in ["/etc/os-release", "/usr/lib/os-release"] {
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                        return name.trim_matches('"').to_string();
                    }
                }
            }
        }
    }

    platform().to_string()
}

fn build_base_request(cfg: &DiscoverConfig) -> SyncRequest {
    SyncRequest {
        id: cfg.agent_id.to_string(),
        name: cfg.name.clone(),
        endpoint: cfg.agent_endpoint.clone(),
        platform: platform().to_string(),
        arch: arch().to_string(),
        os: os_info(),
        metadata: cfg.metadata.clone(),
        ts: 0,
        uptime_seconds: 0,
        endpoint_type: cfg.transport.as_proto(),
        api_version: cfg.api_version as i32,
        heartbeat_interval_s: (cfg.delay_ms / 1000).max(1) as i32,
        capabilities: cfg.capabilities.clone(),
    }
}

fn stamp_request(base: &SyncRequest, uptime: &dyn UptimeSource) -> SyncRequest {
    SyncRequest {
        ts: now_unix_seconds() as i64,
        uptime_seconds: uptime.uptime_seconds() as i64,
        ..base.clone()
    }
}

/// Unix timestamp in seconds. Returns `0` if the system clock is before the epoch
/// (unreachable in practice, but avoids panicking inside a supervised task).
fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn startup_jitter_ms(max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let pid = std::process::id() as u64;
    let mixed = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(pid.rotate_left(32));
    mixed % max_ms
}

/// Compute remaining wait until a server-advised retry deadline.
///
/// Returns `Some(wait)` when `hold_until_unix_s` is in the future, `None` otherwise.
/// `hold_until_unix_s == 0` is treated as "no active hold".
fn compute_hold_wait(hold_until_unix_s: u64, now_unix_s: u64) -> Option<Duration> {
    if hold_until_unix_s == 0 || hold_until_unix_s <= now_unix_s {
        return None;
    }
    Some(Duration::from_secs(hold_until_unix_s - now_unix_s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "http")]
    #[test]
    fn attempt_timeout_covers_jitter_hold_and_request() {
        // The attempt body may legitimately sleep through startup jitter (up to
        // delay_ms) plus a server-advised retry hold (up to MAX_RETRY_AFTER_S)
        // before even sending the request. The per-attempt timeout must cover
        // that, or taskvisor kills healthy heartbeats with AttemptTimedOut cycles.
        let delay_ms = 30_000u64;
        let config = crate::DiscoverConfig::builder(
            solti_model::AgentId::from("agent-1"),
            "agent-1",
            "http://127.0.0.1:8085",
            "http://127.0.0.1:9000",
            crate::DiscoveryTransport::Http,
            delay_ms,
            1,
        )
        .build()
        .expect("config builds");

        let worst_case_ms = delay_ms
            + (MAX_RETRY_AFTER_S as u64) * 1_000
            + config.connect_timeout_ms
            + config.request_timeout_ms;

        let (_task_ref, task) = sync(config, Arc::new(|| 0)).expect("sync builds");
        assert!(
            task.spec().timeout().as_millis() >= worst_case_ms,
            "attempt timeout {}ms must cover the worst case {}ms",
            task.spec().timeout().as_millis(),
            worst_case_ms
        );
    }

    #[test]
    fn compute_hold_wait_zero_means_no_hold() {
        assert_eq!(compute_hold_wait(0, 1_000), None);
        assert_eq!(compute_hold_wait(0, 0), None);
    }

    #[test]
    fn compute_hold_wait_expired_returns_none() {
        assert_eq!(compute_hold_wait(999, 1_000), None);
        assert_eq!(compute_hold_wait(1_000, 1_000), None);
    }

    #[test]
    fn compute_hold_wait_future_returns_remaining() {
        assert_eq!(
            compute_hold_wait(1_060, 1_000),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            compute_hold_wait(1_001, 1_000),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn retry_after_is_clamped_to_max() {
        let raw = i32::MAX;
        let clamped = raw.clamp(0, MAX_RETRY_AFTER_S);
        assert_eq!(clamped, MAX_RETRY_AFTER_S);
        assert_eq!(clamped, 3_600);
        assert_eq!((-10_i32).clamp(0, MAX_RETRY_AFTER_S), 0);
        assert_eq!((120_i32).clamp(0, MAX_RETRY_AFTER_S), 120);
    }

    #[test]
    fn auth_failed_is_terminal() {
        let e = DiscoverError::AuthFailed {
            reason: "http 401".into(),
        };
        assert!(e.is_terminal(), "auth errors must be escalated to Fatal");
    }

    #[test]
    fn transient_errors_are_not_terminal() {
        #[cfg(feature = "http")]
        {
            let e = DiscoverError::HttpStatus {
                code: 503,
                body: "overloaded".into(),
            };
            assert!(!e.is_terminal(), "5xx is transient; sync must retry");
        }
        let e = DiscoverError::Rejected {
            reason: "overloaded".into(),
            retry_after_s: Some(60),
        };
        assert!(!e.is_terminal());
    }

    #[test]
    fn invalid_config_is_terminal() {
        let e = DiscoverError::InvalidConfig("bad endpoint".into());
        assert!(e.is_terminal());
    }

    #[test]
    fn startup_jitter_is_bounded() {
        for max in [1u64, 100, 1_000, 30_000, u64::MAX / 2] {
            let j = startup_jitter_ms(max);
            assert!(j < max, "jitter {j} must be < max {max}");
        }
        assert_eq!(startup_jitter_ms(0), 0);
    }

    #[test]
    fn startup_jitter_varies_between_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            seen.insert(startup_jitter_ms(1_000_000));
            std::thread::sleep(std::time::Duration::from_micros(1));
        }
        assert!(
            seen.len() > 50,
            "jitter should vary between calls; got only {} distinct values out of 100",
            seen.len()
        );
    }

    #[cfg(feature = "http")]
    fn test_config() -> crate::DiscoverConfig {
        crate::DiscoverConfig::builder(
            solti_model::AgentId::from("agent-1"),
            "agent-1",
            "http://127.0.0.1:8085",
            "http://127.0.0.1:9000",
            crate::DiscoveryTransport::Http,
            30_000,
            1,
        )
        .build()
        .expect("config builds")
    }

    #[cfg(feature = "http")]
    #[test]
    fn stamp_request_reads_the_injected_uptime_source() {
        let base = build_base_request(&test_config());
        let source = || 42;

        let request = stamp_request(&base, &source);

        assert_eq!(request.uptime_seconds, 42);
    }
}
