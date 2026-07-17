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

use std::time::Instant;

use tracing::{debug, warn};

use solti_core::uptime_seconds;
use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;
use crate::metrics::{self, DiscoverMetricsHandle};

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
/// - [`DiscoverError::HttpRequest`]: the `reqwest::Client` builder failed (feature `http`; rare - e.g. TLS backend init).
/// - [`DiscoverError::InvalidConfig`]: the TLS config could not be converted into a `rustls` client config (features `http` + `tls`).
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

    // The per-attempt timeout must cover everything the attempt body may
    // legitimately wait through: one-time startup jitter (up to delay_ms),
    // a server-advised retry hold (up to MAX_RETRY_AFTER_S), and the request
    // itself. Tying it to the heartbeat period would kill healthy attempts.
    let attempt_timeout_ms = delay_ms
        .saturating_add((MAX_RETRY_AFTER_S as u64).saturating_mul(1_000))
        .saturating_add(config.connect_timeout_ms)
        .saturating_add(config.request_timeout_ms)
        .saturating_add(1_000);

    let spec = TaskSpec::builder(SLOT, TaskKind::Embedded, attempt_timeout_ms)
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

    #[cfg(feature = "http")]
    let http_client = build_http_client(&config)?;

    let metrics = config.metrics.clone();

    let ctx = Arc::new(SyncContext {
        base_request,
        #[cfg(feature = "http")]
        http_client,
        #[cfg(feature = "grpc")]
        grpc_client: tokio::sync::OnceCell::new(),
        retry_hold_until: AtomicU64::new(0),
        startup_jitter_applied: AtomicBool::new(false),
        metrics,
        config,
    });

    let task: TaskRef = TaskFn::arc(SLOT, move |cancel: TaskContext| {
        let ctx = Arc::clone(&ctx);

        async move {
            if !ctx.startup_jitter_applied.swap(true, Ordering::Relaxed) {
                let jitter = Duration::from_millis(startup_jitter_ms(ctx.config.delay_ms));
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
            let result = cancel.run_until_cancelled(invoke_sync(&ctx)).await?;
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
    /// `0` means no active hold. Updated to `now + retry_after_s` when the control plane returns `Rejected { retry_after_s: Some(_) }`;
    /// cleared to `0` on successful sync.
    retry_hold_until: AtomicU64,
    /// Guard that makes the first-tick startup jitter run exactly once per process lifetime.
    /// Set to `true` after the initial jitter sleep;
    /// subsequent restarts (e.g. after a transient failure) skip the jitter and stay on the periodic schedule.
    startup_jitter_applied: AtomicBool,
    metrics: DiscoverMetricsHandle,
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

async fn invoke_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    match ctx.config.transport {
        #[cfg(feature = "grpc")]
        DiscoveryTransport::Grpc => invoke_grpc_sync(ctx).await,
        #[cfg(feature = "http")]
        DiscoveryTransport::Http => invoke_http_sync(ctx).await,
    }
}

/// Convert [`solti_tls::ClientTlsConfig`] into [`tonic::transport::ClientTlsConfig`].
///
/// Reads PEM bytes via [`solti_tls::PemSource`] and re-shapes them into the PEM-blob types that tonic expects (`Certificate::from_pem`, `Identity::from_pem`).
/// tonic builds its own internal `rustls::ClientConfig`: we cannot pass a pre-built one through.
#[cfg(all(feature = "grpc", feature = "tls"))]
fn build_tonic_client_tls(
    cfg: &solti_tls::ClientTlsConfig,
) -> Result<tonic::transport::ClientTlsConfig, DiscoverError> {
    use tonic::transport::{Certificate, ClientTlsConfig as TonicTls, Identity};

    let ca_bytes = cfg
        .ca
        .read()
        .map_err(|e| DiscoverError::InvalidConfig(format!("read ca pem: {e}")))?;

    let mut tls = TonicTls::new().ca_certificate(Certificate::from_pem(ca_bytes));

    if let (Some(cert_src), Some(key_src)) = (&cfg.client_cert, &cfg.client_key) {
        let cert_bytes = cert_src
            .read()
            .map_err(|e| DiscoverError::InvalidConfig(format!("read client cert pem: {e}")))?;
        let key_bytes = key_src
            .read()
            .map_err(|e| DiscoverError::InvalidConfig(format!("read client key pem: {e}")))?;
        tls = tls.identity(Identity::from_pem(cert_bytes, key_bytes));
    }

    Ok(tls)
}

#[cfg(feature = "grpc")]
async fn invoke_grpc_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let client =
        ctx.grpc_client
            .get_or_try_init(|| async {
                #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
                let mut endpoint = tonic::transport::Endpoint::from_shared(
                    ctx.config.control_plane_endpoint.clone(),
                )
                .map_err(|e| {
                    DiscoverError::InvalidConfig(format!("invalid control_plane_endpoint: {}", e))
                })?
                .connect_timeout(Duration::from_millis(ctx.config.connect_timeout_ms))
                .timeout(Duration::from_millis(ctx.config.request_timeout_ms));

                #[cfg(feature = "tls")]
                if let Some(tls) = &ctx.config.tls {
                    let tonic_tls = build_tonic_client_tls(tls)?;
                    endpoint = endpoint
                        .tls_config(tonic_tls)
                        .map_err(|e| DiscoverError::InvalidConfig(format!("tls_config: {e}")))?;
                }

                let channel = endpoint.connect().await?;
                Ok::<_, DiscoverError>(DiscoverServiceClient::new(channel))
            })
            .await?;

    let mut client = client.clone();
    let mut request = tonic::Request::new(stamp_request(&ctx.base_request));
    if let Some(token) = &ctx.config.token {
        let value: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
            format!("Bearer {}", token.expose()).parse().map_err(|_| {
                DiscoverError::InvalidConfig(
                    "token contains characters invalid for an Authorization header".into(),
                )
            })?;
        request.metadata_mut().insert("authorization", value);
    }
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

/// Build the `reqwest` client used by the HTTP transport.
///
/// Redirects are disabled: the control-plane endpoint is operator-configured,
/// so following redirects is never needed and would only widen the attack
/// surface (e.g. replaying the bearer token to a redirect target).
#[cfg(feature = "http")]
fn build_http_client(config: &DiscoverConfig) -> Result<reqwest::Client, DiscoverError> {
    #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
        .timeout(Duration::from_millis(config.request_timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT);

    #[cfg(feature = "tls")]
    if let Some(tls) = &config.tls {
        // `into_rustls_config` consumes the value; clone to keep the original on `config` (gRPC path also uses it).
        let rustls_cfg = tls
            .clone()
            .into_rustls_config()
            .map_err(|e| DiscoverError::InvalidConfig(format!("tls into_rustls_config: {e}")))?;
        builder = builder.use_preconfigured_tls(rustls_cfg);
    }

    Ok(builder.build()?)
}

#[cfg(feature = "http")]
async fn invoke_http_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let request = stamp_request(&ctx.base_request);

    let url = format!(
        "{}{}",
        ctx.config.control_plane_endpoint,
        http_sync_path(ctx.config.api_version),
    );
    let mut http_req = ctx.http_client.post(url).json(&request);
    if let Some(token) = &ctx.config.token {
        http_req = http_req.bearer_auth(token.expose());
    }
    let response = http_req.send().await?;

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

/// Turn a `success=false` response into [`DiscoverError::Rejected`].
///
/// `reason` is propagated **verbatim** from the control plane:
/// treat it as untrusted server text (do not interpolate it into anything trust-sensitive).
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
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<String, DiscoverError> {
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(DiscoverError::InvalidResponse(format!(
            "response body {len} bytes exceeds cap {max_bytes}"
        )));
    }

    // Without a trustworthy Content-Length (e.g. chunked transfer encoding) the
    // body must be read incrementally: buffering it whole before the cap check
    // would let the server force an arbitrarily large allocation.
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(DiscoverError::InvalidResponse(format!(
                "response body exceeds cap {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body)
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

        let (_task, spec) = sync(config).expect("sync builds");
        assert!(
            spec.timeout().as_millis() >= worst_case_ms,
            "attempt timeout {}ms must cover the worst case {}ms",
            spec.timeout().as_millis(),
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

    /// Raw TCP stub serving one hand-written HTTP/1.1 response, then closing.
    #[cfg(feature = "http")]
    async fn one_shot_http_stub(response: &'static [u8]) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket.write_all(response).await.expect("write response");
        });
        addr
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
    #[tokio::test]
    async fn read_body_bounded_rejects_oversized_chunked_body_before_stream_ends() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");

        // Endless chunked response: no Content-Length, no terminating 0-chunk.
        // The stub only stops once the client hangs up, so an implementation
        // that buffers the whole body before checking the cap never returns.
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let payload = [b'a'; 4096];
            loop {
                // 0x1000 == 4096-byte chunk.
                if socket.write_all(b"1000\r\n").await.is_err()
                    || socket.write_all(&payload).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                    || socket.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let response = build_http_client(&test_config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("stub accepts the request");

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            read_body_bounded(response, MAX_RESPONSE_BODY_BYTES),
        )
        .await
        .expect("cap must trip while the stream is still open, not after buffering it");

        match result {
            Err(DiscoverError::InvalidResponse(msg)) => {
                assert!(msg.contains("exceeds cap"), "unexpected message: {msg}");
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn read_body_bounded_accepts_chunked_body_within_cap() {
        let addr = one_shot_http_stub(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              b\r\nhello world\r\n0\r\n\r\n",
        )
        .await;

        let response = build_http_client(&test_config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("stub accepts the request");

        let body = read_body_bounded(response, MAX_RESPONSE_BODY_BYTES)
            .await
            .expect("small chunked body is accepted");
        assert_eq!(body, "hello world");
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn http_client_does_not_follow_redirects() {
        // Location points at a closed port: a client that follows the redirect
        // fails with a connect error instead of returning this 302 response.
        let addr = one_shot_http_stub(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;

        let response = build_http_client(&test_config())
            .expect("client builds")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("redirect must be returned, not followed");

        assert_eq!(response.status().as_u16(), 302);
    }
}
