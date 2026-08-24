//! # Discovery sync
//!
//! ```text
//! first attempt
//!      │ startup jitter
//!      ▼
//! server hold ──► stamp time and uptime ──► HTTP or gRPC sync
//!                    ┌───────────────────────────┴───────────────────────────┐
//!                    ▼                                                       ▼
//!                 success                                           discovery error
//!                    │                                                       │
//!             periodic delay                              retryable ──► TaskError::Fail
//!                                                        permanent ──► TaskError::Fatal
//! ```
//!
//! Startup jitter runs only before the first attempt.
//! An active server-advised hold is checked before each request.
//! Each request receives a fresh timestamp and uptime value.
//! Taskvisor schedules retry backoff between task attempts.
//!
//! The returned manifest uses slot `solti-discover-sync`.
//! It uses `AdmissionPolicy::Replace` and a periodic restart policy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Maximum server-advised hold in seconds.
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
/// The selected transport adapter is created before this function returns.
/// Network connections remain lazy.
///
/// The returned task captures `config` and `uptime`.
/// Submit both returned values through the embedded task API in `solti-core`.
///
/// # Errors
///
/// Returns [`DiscoverError::InvalidConfig`] when the selected transport cannot use the config.
/// This includes a bearer token over plaintext transport without the explicit insecure opt-in.
/// Returns [`DiscoverError::SpecBuild`] when the manifest cannot be built.
/// With HTTP, returns a transport error when the client cannot be built.
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
            event = "discovery.insecure_transport",
            "bearer token explicitly allowed over plaintext discovery"
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

    let task: TaskRef = TaskFn::arc(move |cancel: TaskContext| {
        let ctx = Arc::clone(&ctx);

        async move {
            if !ctx.startup_jitter_applied.swap(true, Ordering::Relaxed) {
                let jitter = Duration::from_millis(startup_jitter_ms(ctx.delay_ms));
                debug!(
                    event = "discovery.startup_jitter",
                    jitter_ms = jitter.as_millis() as u64,
                    "discovery startup delayed",
                );
                cancel
                    .run_until_cancelled(tokio::time::sleep(jitter))
                    .await?;
            }

            if let Some(wait) = ctx.retry_hold_wait() {
                debug!(
                    event = "discovery.retry_hold",
                    wait_s = wait.as_secs(),
                    "discovery retry delayed"
                );
                cancel.run_until_cancelled(tokio::time::sleep(wait)).await?;
            }

            let request = stamp_request(&ctx.base_request, ctx.uptime.as_ref())
                .map_err(TaskError::fatal_from)?;

            debug!(
                event = "discovery.sync",
                stage = "started",
                "discovery sync started"
            );
            metrics::record_attempt(&ctx.metrics);
            let start = Instant::now();
            let result = cancel
                .run_until_cancelled(ctx.transport.sync(request))
                .await?;
            let duration_ms = start.elapsed().as_millis() as u64;
            match result {
                Ok(()) => {
                    metrics::record_success(&ctx.metrics, duration_ms);
                    ctx.clear_retry_hold();
                    debug!(
                        event = "discovery.sync",
                        stage = "completed",
                        duration_ms,
                        "discovery sync completed"
                    );
                    Ok(())
                }
                Err(e) => {
                    let failure = classify_failure(&e);
                    metrics::record_failure(&ctx.metrics, duration_ms, failure);
                    if let DiscoverError::Rejected {
                        retry_after_s: Some(s),
                        ..
                    } = &e
                    {
                        let clamped = clamp_retry_after_s(*s);
                        if *s != clamped {
                            warn!(
                                event = "discovery.retry_hold_capped",
                                advised_s = *s,
                                capped_s = clamped,
                                "discovery retry hold capped"
                            );
                        }
                        ctx.set_retry_hold(Duration::from_secs(clamped as u64));
                        metrics::record_hold(&ctx.metrics, clamped as u64);
                    }

                    match e.retryability() {
                        Retryability::Permanent => {
                            warn!(
                                event = "discovery.sync_failed",
                                error_kind = failure.as_label(),
                                retryable = false,
                                duration_ms,
                                "discovery sync failed"
                            );
                            Err(TaskError::fatal_from(e))
                        }
                        Retryability::Retryable => {
                            debug!(
                                event = "discovery.sync_failed",
                                error_kind = failure.as_label(),
                                retryable = true,
                                duration_ms,
                                "discovery sync failed"
                            );
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

/// Returns the canonical metric label for a [`DiscoverError`].
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

/// Returns the platform description sent in discovery metadata.
///
/// Linux uses `PRETTY_NAME` from `/etc/os-release` when available.
/// It falls back to `/usr/lib/os-release`, then the Rust platform name.
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

fn stamp_request(
    base: &SyncRequest,
    uptime: &dyn UptimeSource,
) -> Result<SyncRequest, DiscoverError> {
    let ts = i64::try_from(now_unix_seconds()).map_err(|_| {
        DiscoverError::InvalidConfig(
            "current Unix timestamp exceeds the discovery v1 wire range".into(),
        )
    })?;
    let uptime_seconds = i64::try_from(uptime.uptime_seconds()).map_err(|_| {
        DiscoverError::InvalidConfig("uptime_seconds exceeds the discovery v1 wire range".into())
    })?;

    Ok(SyncRequest {
        ts,
        uptime_seconds,
        ..base.clone()
    })
}

/// Returns the current Unix timestamp in seconds.
///
/// Returns zero when the system clock is before the Unix epoch.
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

fn clamp_retry_after_s(seconds: i32) -> i32 {
    seconds.clamp(0, MAX_RETRY_AFTER_S)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{Labels, RunnerCapability, WORKLOAD_API_VERSION, WorkloadTypeMeta};

    #[cfg(feature = "http")]
    use std::sync::atomic::AtomicUsize;

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
            solti_model::AgentCapabilities::default(),
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

    #[cfg(feature = "http")]
    #[test]
    fn sync_rejects_plaintext_http_token_without_opt_in() {
        let config = DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            solti_model::AgentCapabilities::default(),
            30_000,
            "test@1",
        )
        .with_token(solti_model::Token::new("secret").unwrap())
        .build()
        .unwrap();

        assert!(matches!(
            sync(config, Arc::new(|| 0)),
            Err(DiscoverError::InvalidConfig(message))
                if message.contains("allow_insecure_token_transport()")
        ));
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn sync_rejects_h2c_token_without_opt_in() {
        let config = DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:50051",
                crate::DiscoveryTransport::Grpc,
            )
            .unwrap(),
            solti_model::AgentCapabilities::default(),
            30_000,
            "test@1",
        )
        .with_token(solti_model::Token::new("secret").unwrap())
        .build()
        .unwrap();

        assert!(matches!(
            sync(config, Arc::new(|| 0)),
            Err(DiscoverError::InvalidConfig(message))
                if message.contains("allow_insecure_token_transport()")
        ));
    }

    #[test]
    fn compute_hold_wait_handles_absent_expired_and_future_deadlines() {
        let now = Instant::now();

        assert_eq!(compute_hold_wait(None, now), None);
        assert_eq!(compute_hold_wait(Some(now), now), None);
        assert_eq!(
            compute_hold_wait(Some(now - Duration::from_secs(1)), now),
            None
        );
        assert_eq!(
            compute_hold_wait(Some(now + Duration::from_secs(60)), now),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn retry_after_is_clamped_to_max() {
        assert_eq!(clamp_retry_after_s(i32::MAX), MAX_RETRY_AFTER_S);
        assert_eq!(clamp_retry_after_s(-10), 0);
        assert_eq!(clamp_retry_after_s(120), 120);
    }

    #[test]
    fn startup_jitter_is_bounded() {
        for max in [1u64, 100, 1_000, 30_000, u64::MAX / 2] {
            let j = startup_jitter_ms(max);
            assert!(j < max, "jitter {j} must be < max {max}");
        }
        assert_eq!(startup_jitter_ms(0), 0);
    }

    #[cfg(feature = "http")]
    fn test_config() -> DiscoverConfig {
        DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            solti_model::AgentCapabilities::default(),
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

        let request = stamp_request(&base, &source).expect("uptime fits the wire range");

        assert_eq!(request.uptime_seconds, 42);
    }

    #[test]
    fn stamp_request_rejects_uptime_outside_the_wire_range() {
        let source = || u64::MAX;

        let error = stamp_request(&SyncRequest::default(), &source)
            .expect_err("u64::MAX does not fit discovery v1 int64 uptime");

        assert!(matches!(error, DiscoverError::InvalidConfig(message) if
            message == "uptime_seconds exceeds the discovery v1 wire range"));
    }

    #[cfg(feature = "http")]
    #[tokio::test]
    async fn local_wire_stamp_failure_records_no_transport_metrics() {
        #[derive(Debug, Default)]
        struct MetricsProbe {
            attempts: AtomicUsize,
            failures: AtomicUsize,
        }

        impl crate::DiscoverMetricsBackend for MetricsProbe {
            fn record_attempt(&self) {
                self.attempts.fetch_add(1, Ordering::SeqCst);
            }

            fn record_failure(&self, _duration_ms: u64, _reason: crate::DiscoverFailReason) {
                self.failures.fetch_add(1, Ordering::SeqCst);
            }
        }

        let metrics = Arc::new(MetricsProbe::default());
        let metrics_handle: crate::DiscoverMetricsHandle = metrics.clone();
        let config = DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new("http://127.0.0.1:9", crate::DiscoveryTransport::Http)
                .unwrap(),
            solti_model::AgentCapabilities::default(),
            1,
            "wire-stamp-failure@1",
        )
        .with_metrics(metrics_handle)
        .build()
        .unwrap();
        let (_, task) = sync(config, Arc::new(|| u64::MAX)).unwrap();

        let error = task
            .spawn(TaskContext::detached())
            .await
            .expect_err("out-of-range uptime must fail before transport");

        assert!(
            error
                .to_string()
                .contains("uptime_seconds exceeds the discovery v1 wire range"),
            "{error}"
        );
        assert_eq!(metrics.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(metrics.failures.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "http")]
    #[test]
    fn request_uses_the_advertised_transport_not_the_discovery_transport() {
        let config = DiscoverConfig::builder(
            solti_model::AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("127.0.0.1:50051", crate::AgentEndpointType::Grpc, 7)
                .unwrap(),
            crate::ControlPlaneEndpoint::new(
                "http://127.0.0.1:9000",
                crate::DiscoveryTransport::Http,
            )
            .unwrap(),
            solti_model::AgentCapabilities::default(),
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
