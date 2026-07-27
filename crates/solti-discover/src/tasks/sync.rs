//! Periodic discovery sync task.
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
//! Retryable failures become `TaskError::Fail`. Permanent failures become
//! `TaskError::Fatal`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Upper bound on server-advised hold time (seconds).
const MAX_RETRY_AFTER_S: i32 = 3_600;

use crate::proto::SyncRequest;
use crate::proto_agent::{
    AgentCapabilities as ProtoAgentCapabilities, RunnerCapability as ProtoRunnerCapability,
    WorkloadType as ProtoWorkloadType,
};

use tracing::{debug, warn};

use solti_model::{
    AdmissionPolicy, AgentCapabilities, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy,
    TaskManifest, TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

use crate::config::DiscoverConfig;
use crate::errors::{DiscoverError, Retryability};
use crate::metrics::{self, DiscoverMetricsHandle};
use crate::tasks::transport::TransportAdapter;
use crate::uptime::UptimeSource;

const SLOT: &str = "solti-discover-sync";

/// Builds the embedded heartbeat task and its desired resource.
///
/// # Errors
///
/// Returns [`DiscoverError`] when the task manifest or selected transport
/// cannot be built from the validated config.
pub fn sync(
    config: DiscoverConfig,
    uptime: Arc<dyn UptimeSource>,
) -> Result<(TaskManifest, TaskRef), DiscoverError> {
    let delay_ms = config.delay_ms;

    let backoff = config.backoff.clone().unwrap_or_else(|| BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: (delay_ms / 2).max(1),
        max_ms: delay_ms.saturating_mul(3),
        factor: 2.0,
    });

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

    let transport = TransportAdapter::from_config(&config)?;
    if config.token.is_some() && !transport.is_secure() {
        warn!(
            "discovery: presenting a bearer token over a plaintext channel; \
             enable TLS to protect the credential in transit"
        );
    }

    let base_request = build_base_request(&config);
    let metrics = config.metrics.clone();

    let ctx = Arc::new(SyncContext {
        base_request,
        delay_ms,
        transport,
        retry_hold_until: Mutex::new(None),
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

            if let Some(wait) = ctx.retry_hold_wait() {
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
                    ctx.clear_retry_hold();
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
                        ctx.set_retry_hold(Duration::from_secs(clamped as u64));
                        ctx.metrics.record_hold(clamped as u64);
                    }

                    match e.retryability() {
                        Retryability::Permanent => {
                            warn!("sync failed permanently: {}", e);
                            Err(TaskError::fatal_from(e))
                        }
                        Retryability::Retryable => {
                            warn!("sync failed: {}", e);
                            Err(TaskError::fail_from(e))
                        }
                    }
                }
            }
        }
    });
    let manifest =
        TaskManifest::new(SLOT, spec).map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;
    Ok((manifest, task))
}

struct SyncContext {
    base_request: SyncRequest,
    delay_ms: u64,
    transport: TransportAdapter,
    retry_hold_until: Mutex<Option<Instant>>,
    startup_jitter_applied: AtomicBool,
    metrics: DiscoverMetricsHandle,
    uptime: Arc<dyn UptimeSource>,
}

impl SyncContext {
    fn clear_retry_hold(&self) {
        *self
            .retry_hold_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn retry_hold_wait(&self) -> Option<Duration> {
        let mut deadline = self
            .retry_hold_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wait = compute_hold_wait(*deadline, Instant::now());
        if wait.is_none() {
            *deadline = None;
        }
        wait
    }

    fn set_retry_hold(&self, duration: Duration) {
        *self
            .retry_hold_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Instant::now().checked_add(duration);
    }
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
                | Code::Unimplemented => metrics::DiscoverFailReason::RejectedClient,
                Code::ResourceExhausted | Code::Aborted | Code::Cancelled => {
                    metrics::DiscoverFailReason::RejectedServer
                }
                _ => metrics::DiscoverFailReason::Other,
            }
        }
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
        endpoint: cfg.agent_endpoint.address.clone(),
        platform: platform().to_string(),
        arch: arch().to_string(),
        os: os_info(),
        metadata: cfg.metadata.clone(),
        ts: 0,
        uptime_seconds: 0,
        endpoint_type: cfg.agent_endpoint.endpoint_type.as_proto(),
        api_version: cfg.agent_endpoint.api_version,
        heartbeat_interval_s: cfg.heartbeat_interval_s,
        capabilities: Some(capabilities_to_proto(&cfg.capabilities)),
    }
}

fn capabilities_to_proto(capabilities: &AgentCapabilities) -> ProtoAgentCapabilities {
    ProtoAgentCapabilities {
        runners: capabilities
            .runners()
            .iter()
            .map(|runner| ProtoRunnerCapability {
                name: runner.name().to_owned(),
                labels: runner
                    .labels()
                    .iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
                workloads: runner
                    .workload_types()
                    .iter()
                    .map(|workload| ProtoWorkloadType {
                        api_version: workload.api_version().to_owned(),
                        kind: workload.kind().to_owned(),
                    })
                    .collect(),
            })
            .collect(),
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

fn compute_hold_wait(deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    deadline
        .and_then(|deadline| deadline.checked_duration_since(now))
        .filter(|duration| !duration.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{Labels, RunnerCapability, WORKLOAD_API_VERSION, WorkloadTypeMeta};

    #[test]
    fn capabilities_preserve_runner_name_labels_and_workload_gvks() {
        let mut labels = Labels::new();
        labels.insert("solti.io/runner-name", "secure");
        labels.insert("topology.solti.io/zone", "eu-1");
        let capabilities = AgentCapabilities::new(vec![
            RunnerCapability::new(
                "secure",
                labels,
                vec![
                    WorkloadTypeMeta::new("tasks.example.io/v1", "DatabaseBackup").unwrap(),
                    WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap(),
                ],
            )
            .unwrap(),
        ])
        .unwrap();

        let proto = capabilities_to_proto(&capabilities);

        assert_eq!(proto.runners.len(), 1);
        let runner = &proto.runners[0];
        assert_eq!(runner.name, "secure");
        assert_eq!(
            runner
                .labels
                .get("solti.io/runner-name")
                .map(String::as_str),
            Some("secure"),
        );
        assert_eq!(
            runner
                .labels
                .get("topology.solti.io/zone")
                .map(String::as_str),
            Some("eu-1"),
        );
        assert_eq!(
            runner
                .workloads
                .iter()
                .map(|workload| (workload.api_version.as_str(), workload.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (WORKLOAD_API_VERSION, "Subprocess"),
                ("tasks.example.io/v1", "DatabaseBackup"),
            ],
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn attempt_timeout_covers_jitter_hold_and_request() {
        // The attempt body may legitimately sleep through startup jitter (up to
        // delay_ms) plus a server-advised retry hold (up to MAX_RETRY_AFTER_S)
        // before even sending the request. The per-attempt timeout must cover
        // that, or taskvisor kills healthy heartbeats with AttemptTimedOut cycles.
        let delay_ms = 30_000u64;
        let config = crate::DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            delay_ms,
            "test@1",
        )
        .build()
        .expect("config builds");

        let worst_case_ms = delay_ms
            + (MAX_RETRY_AFTER_S as u64) * 1_000
            + config.connect_timeout_ms
            + config.request_timeout_ms;

        let (manifest, _) = sync(config, Arc::new(|| 0)).expect("sync builds");
        assert!(
            manifest.spec().timeout().as_millis() >= worst_case_ms,
            "attempt timeout {}ms must cover the worst case {}ms",
            manifest.spec().timeout().as_millis(),
            worst_case_ms
        );
    }

    #[test]
    fn compute_hold_wait_none_means_no_hold() {
        assert_eq!(compute_hold_wait(None, Instant::now()), None);
    }

    #[test]
    fn compute_hold_wait_expired_returns_none() {
        let now = Instant::now();
        assert_eq!(compute_hold_wait(Some(now), now), None);
        assert_eq!(
            compute_hold_wait(Some(now - Duration::from_secs(1)), now),
            None
        );
    }

    #[test]
    fn compute_hold_wait_future_returns_remaining() {
        let now = Instant::now();
        assert_eq!(
            compute_hold_wait(Some(now + Duration::from_secs(60)), now),
            Some(Duration::from_secs(60))
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
    fn auth_failed_is_permanent() {
        let e = DiscoverError::AuthFailed {
            reason: "http 401".into(),
        };
        assert_eq!(e.retryability(), Retryability::Permanent);
    }

    #[test]
    fn transient_errors_are_retryable() {
        #[cfg(feature = "http")]
        {
            let e = DiscoverError::HttpStatus {
                code: 503,
                body: "overloaded".into(),
            };
            assert_eq!(e.retryability(), Retryability::Retryable);
        }
        let e = DiscoverError::Rejected {
            reason: "overloaded".into(),
            retry_after_s: Some(60),
        };
        assert_eq!(e.retryability(), Retryability::Retryable);
    }

    #[test]
    fn invalid_config_is_permanent() {
        let e = DiscoverError::InvalidConfig("bad endpoint".into());
        assert_eq!(e.retryability(), Retryability::Permanent);
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
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            30_000,
            "test@1",
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

    #[cfg(feature = "http")]
    #[test]
    fn request_uses_the_advertised_transport_not_the_discovery_transport() {
        let config = crate::DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("127.0.0.1:50051", crate::AgentEndpointType::Grpc, 7)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            30_000,
            "test@1",
        )
        .build()
        .unwrap();

        let request = build_base_request(&config);
        assert_eq!(
            request.endpoint_type,
            crate::proto::EndpointType::Grpc as i32
        );
        assert_eq!(request.api_version, 7);
    }
}
