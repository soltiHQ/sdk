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

#[cfg(feature = "grpc")]
use crate::proto::discover_service_client::DiscoverServiceClient;
use crate::proto::{SyncRequest, SyncResponse};

#[cfg(feature = "http")]
const MAX_BODY_PREVIEW_BYTES: usize = 1024;

#[cfg(feature = "http")]
const MAX_RESPONSE_BODY_BYTES: u64 = 64 * 1024;

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use solti_core::uptime_seconds;
use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskError, TaskFn, TaskRef};

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;

const SLOT: &str = "solti-discover-sync";

/// User-Agent sent by the HTTP transport: `"solti-discover/<version>"`.
#[cfg(feature = "http")]
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Build a heartbeat task and its spec from discovery config.
///
/// Returns `(TaskRef, TaskSpec)` ready for `SupervisorApi::submit_with_task`.
///
/// ## Errors
///
/// - [`DiscoverError::SpecBuild`]: `TaskSpec` validation failed.
/// - [`DiscoverError::HttpRequest`] `reqwest::Client` builder failed (rare; e.g. invalid default timeouts or TLS backend init).
///
/// ## Also
///
/// - [`DiscoverConfig`](crate::DiscoverConfig) controls endpoint, transport, interval.
/// - [`DiscoverError`](crate::DiscoverError) failure modes surfaced via `TaskError::Fail`.
pub fn sync(config: DiscoverConfig) -> Result<(TaskRef, TaskSpec), DiscoverError> {
    let delay_ms = config.delay_ms;

    let backoff = config.backoff.clone().unwrap_or_else(|| BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: (delay_ms / 2).max(1),
        max_ms: delay_ms.saturating_mul(3),
        factor: 2.0,
    });

    let spec = TaskSpec::builder(SLOT, TaskKind::Embedded, delay_ms)
        .restart(RestartPolicy::periodic(delay_ms))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;

    let base_request = build_base_request(&config);

    #[cfg(feature = "http")]
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
        .timeout(Duration::from_millis(config.request_timeout_ms))
        .user_agent(USER_AGENT)
        .build()?;

    let ctx = Arc::new(SyncContext {
        base_request,
        #[cfg(feature = "http")]
        http_client,
        #[cfg(feature = "grpc")]
        grpc_client: tokio::sync::OnceCell::new(),
        retry_hold_until: AtomicU64::new(0),
        startup_jitter_applied: AtomicBool::new(false),
        config,
    });

    let task: TaskRef = TaskFn::arc(SLOT, move |cancel: CancellationToken| {
        let ctx = Arc::clone(&ctx);

        async move {
            if !ctx.startup_jitter_applied.swap(true, Ordering::Relaxed) {
                let jitter = Duration::from_millis(startup_jitter_ms(ctx.config.delay_ms));
                debug!(
                    jitter_ms = jitter.as_millis() as u64,
                    "applying startup jitter before first sync",
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Err(TaskError::Canceled),
                    _ = tokio::time::sleep(jitter) => {}
                }
            }

            if let Some(wait) = compute_hold_wait(
                ctx.retry_hold_until.load(Ordering::Relaxed),
                now_unix_seconds(),
            ) {
                debug!(
                    wait_s = wait.as_secs(),
                    "waiting for server-advised retry hold"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Err(TaskError::Canceled),
                    _ = tokio::time::sleep(wait) => {}
                }
            }

            debug!("sending sync request to control plane");
            tokio::select! {
                _ = cancel.cancelled() => Err(TaskError::Canceled),
                result = invoke_sync(&ctx) => match result {
                    Ok(()) => {
                        ctx.retry_hold_until.store(0, Ordering::Relaxed);
                        debug!("sync completed successfully");
                        Ok(())
                    }
                    Err(e) => {
                        if let DiscoverError::Rejected {
                            retry_after_s: Some(s),
                            ..
                        } = &e
                        {
                            let clamped = (*s).clamp(0, MAX_RETRY_AFTER_S);
                            if *s != clamped {
                                warn!(
                                    advised_s = *s,
                                    capped_s = clamped,
                                    "retry_after_s capped",
                                );
                            }
                            let hold_until =
                                now_unix_seconds().saturating_add(clamped as u64);
                            ctx.retry_hold_until.store(hold_until, Ordering::Relaxed);
                        }

                        if e.is_terminal() {
                            warn!("sync failed fatally: {}", e);
                            Err(TaskError::Fatal {
                                reason: format!("sync fatally failed: {}", e),
                                exit_code: None,
                            })
                        } else {
                            warn!("sync failed: {}", e);
                            Err(TaskError::Fail {
                                reason: format!("sync failed: {}", e),
                                exit_code: None,
                            })
                        }
                    }
                },
            }
        }
    });
    Ok((task, spec))
}

struct SyncContext {
    config: DiscoverConfig,
    base_request: SyncRequest,
    #[cfg(feature = "http")]
    http_client: reqwest::Client,
    #[cfg(feature = "grpc")]
    grpc_client: tokio::sync::OnceCell<DiscoverServiceClient<tonic::transport::Channel>>,
    /// Unix timestamp (seconds) before which the next sync attempt must wait.
    ///
    /// `0` means no active hold. Updated to `now + retry_after_s` when the control plane
    /// returns `Rejected { retry_after_s: Some(_) }`; cleared to `0` on successful sync.
    retry_hold_until: AtomicU64,
    /// Guard so the first-tick startup jitter runs exactly once per process
    /// lifetime. Set to `true` after the initial jitter sleep; subsequent
    /// restarts (e.g. after a transient failure) skip the jitter and stay
    /// on the periodic schedule.
    startup_jitter_applied: AtomicBool,
}

async fn invoke_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    match ctx.config.transport {
        #[cfg(feature = "grpc")]
        DiscoveryTransport::Grpc => invoke_grpc_sync(ctx).await,
        #[cfg(feature = "http")]
        DiscoveryTransport::Http => invoke_http_sync(ctx).await,
    }
}

#[cfg(feature = "grpc")]
async fn invoke_grpc_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let client =
        ctx.grpc_client
            .get_or_try_init(|| async {
                let endpoint = tonic::transport::Endpoint::from_shared(
                    ctx.config.control_plane_endpoint.clone(),
                )
                .map_err(|e| {
                    DiscoverError::InvalidConfig(format!("invalid control_plane_endpoint: {}", e))
                })?
                .connect_timeout(Duration::from_millis(ctx.config.connect_timeout_ms))
                .timeout(Duration::from_millis(ctx.config.request_timeout_ms));
                let channel = endpoint.connect().await?;
                Ok::<_, DiscoverError>(DiscoverServiceClient::new(channel))
            })
            .await?;

    let mut client = client.clone();
    let request = tonic::Request::new(stamp_request(&ctx.base_request));
    match client.sync(request).await {
        Ok(response) => validate_response(response.into_inner()),
        Err(status) => match status.code() {
            tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
                Err(DiscoverError::AuthFailed {
                    reason: format!("grpc {:?}: {}", status.code(), status.message()),
                })
            }
            _ => Err(DiscoverError::from(status)),
        },
    }
}

#[cfg(feature = "http")]
async fn invoke_http_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let request = stamp_request(&ctx.base_request);

    let url = format!(
        "{}{}",
        ctx.config.control_plane_endpoint,
        http_sync_path(ctx.config.api_version),
    );
    let response = ctx.http_client.post(url).json(&request).send().await?;

    let status = response.status();
    let body = read_body_bounded(response, MAX_RESPONSE_BODY_BYTES).await?;

    if !status.is_success() {
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(DiscoverError::AuthFailed {
                reason: format!("http {}: {}", status.as_u16(), truncate_body(&body)),
            });
        }
        return Err(DiscoverError::HttpStatus {
            code: status.as_u16(),
            body: truncate_body(&body),
        });
    }

    let sync_response: SyncResponse = serde_json::from_str(&body).map_err(|e| {
        DiscoverError::InvalidResponse(format!(
            "failed to parse response: {}, body: {}",
            e,
            truncate_body(&body)
        ))
    })?;

    validate_response(sync_response)
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

fn stamp_request(base: &SyncRequest) -> SyncRequest {
    SyncRequest {
        ts: now_unix_seconds() as i64,
        uptime_seconds: uptime_seconds() as i64,
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

fn validate_response(response: SyncResponse) -> Result<(), DiscoverError> {
    if !response.success {
        let reason = if response.reason.is_empty() {
            "control plane returned success=false".to_string()
        } else {
            response.reason
        };
        let retry_after_s = if response.retry_after_s > 0 {
            Some(response.retry_after_s)
        } else {
            None
        };
        return Err(DiscoverError::Rejected {
            reason,
            retry_after_s,
        });
    }
    Ok(())
}

/// HTTP path for the sync RPC, derived from `api_version`.
///
/// `api_version = 1` → `"/api/v1/discovery/sync"`.
/// Ensures the wire path and the `api_version` field of [`SyncRequest`] stay in lockstep.
#[cfg(feature = "http")]
fn http_sync_path(api_version: u32) -> String {
    format!("/api/v{api_version}/discovery/sync")
}

#[cfg(feature = "http")]
async fn read_body_bounded(
    response: reqwest::Response,
    max_bytes: u64,
) -> Result<String, DiscoverError> {
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(DiscoverError::InvalidResponse(format!(
            "response body {len} bytes exceeds cap {max_bytes}"
        )));
    }

    let bytes = response.bytes().await?;
    if bytes.len() as u64 > max_bytes {
        return Err(DiscoverError::InvalidResponse(format!(
            "response body {} bytes exceeds cap {max_bytes}",
            bytes.len()
        )));
    }

    String::from_utf8(bytes.to_vec())
        .map_err(|e| DiscoverError::InvalidResponse(format!("response body is not UTF-8: {e}")))
}

/// Truncate a response body preview at a char boundary, capping at ~1 KiB.
#[cfg(feature = "http")]
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_BODY_PREVIEW_BYTES {
        return body.to_string();
    }
    let mut end = MAX_BODY_PREVIEW_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = body[..end].to_string();
    truncated.push_str("... [truncated]");
    truncated
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

    #[cfg(feature = "http")]
    #[test]
    fn http_sync_path_derives_from_api_version() {
        assert_eq!(http_sync_path(1), "/api/v1/discovery/sync");
        assert_eq!(http_sync_path(2), "/api/v2/discovery/sync");
        assert_eq!(http_sync_path(42), "/api/v42/discovery/sync");
    }

    #[test]
    fn validate_response_success_ok() {
        let r = SyncResponse {
            success: true,
            reason: String::new(),
            retry_after_s: 0,
        };
        assert!(validate_response(r).is_ok());
    }

    #[test]
    fn validate_response_rejection_without_reason_uses_default() {
        let r = SyncResponse {
            success: false,
            reason: String::new(),
            retry_after_s: 0,
        };
        match validate_response(r) {
            Err(DiscoverError::Rejected {
                reason,
                retry_after_s,
            }) => {
                assert!(reason.contains("success=false"));
                assert_eq!(retry_after_s, None);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn validate_response_rejection_with_hint_is_preserved() {
        let r = SyncResponse {
            success: false,
            reason: "overloaded".into(),
            retry_after_s: 60,
        };
        match validate_response(r) {
            Err(DiscoverError::Rejected {
                reason,
                retry_after_s,
            }) => {
                assert_eq!(reason, "overloaded");
                assert_eq!(retry_after_s, Some(60));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn validate_response_rejection_negative_hint_is_dropped() {
        let r = SyncResponse {
            success: false,
            reason: "bad".into(),
            retry_after_s: -5,
        };
        match validate_response(r) {
            Err(DiscoverError::Rejected { retry_after_s, .. }) => {
                assert_eq!(retry_after_s, None);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
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
}
