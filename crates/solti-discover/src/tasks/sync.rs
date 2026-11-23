//! # Discovery sync
//!
//! ```text
//! first attempt
//!      │ startup jitter
//!      ▼
//! server hold ──► stamp time and uptime ──► heartbeat, never with a schema
//!                    ┌──────────────────────────────┴──────────────────────────────┐
//!                    ▼                                                             ▼
//!                 success                                                  discovery error
//!                    │                                                             │
//!    requested schemas left to send ──► follow-up, at most 6       retryable ──► TaskError::Fail
//!                    │                     └── failure ──► WARN, Ok   permanent ──► TaskError::Fatal
//!                    ▼
//!             periodic delay
//! ```
//!
//! Startup jitter runs only before the first attempt.
//! An active server-advised hold is checked before each attempt.
//! Each request receives a fresh timestamp and uptime value.
//! Taskvisor schedules retry backoff between task attempts.
//!
//! The heartbeat decides the attempt and never carries a schema, so schema
//! delivery can never fail it. Its response restates which schemas the control
//! plane wants. Follow-ups then carry the ones that fit the encoded request
//! budget; [`SchemaExchange`] chooses them. A failed follow-up is logged and
//! leaves the attempt successful.
//!
//! The returned manifest uses slot `solti-discover-sync`.
//! It uses `AdmissionPolicy::Replace` and a periodic restart policy.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Maximum server-advised hold in seconds.
const MAX_RETRY_AFTER_S: i32 = 3_600;

/// Most unknown `want_schemas` entries one log event shows.
const MAX_LOGGED_UNKNOWN: usize = 8;

/// Most bytes of one unknown `want_schemas` entry a log event shows.
const MAX_LOGGED_UNKNOWN_BYTES: usize = 128;

use crate::proto::{SyncRequest, SyncResponse};
use crate::proto_agent::{
    AgentCapabilities as ProtoAgentCapabilities, RunnerCapability as ProtoRunnerCapability,
    WorkloadType as ProtoWorkloadType,
};

use tracing::{debug, info, warn};

use solti_model::{
    AdmissionPolicy, AgentCapabilities, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy,
    SchemaFingerprint, TaskManifest, TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::{DiscoverError, Retryability};
use crate::metrics::{self, DiscoverFailReason, DiscoverMetricsHandle};
use crate::tasks::schemas::{
    MAX_SCHEMA_FOLLOW_UPS, MAX_SYNC_REQUEST_BYTES, SYNC_REQUEST_BUDGET,
    SYNC_REQUEST_HEADROOM_BYTES, SchemaExchange,
};
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
/// It also includes a heartbeat, which never carries a schema, that encodes over the request
/// budget for the selected transport: 256 KiB minus 1 KiB of headroom.
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

    // The heartbeat and each follow-up may take the connect and the request timeout.
    let requests_ms = (1 + MAX_SCHEMA_FOLLOW_UPS as u64).saturating_mul(
        config
            .connect_timeout_ms
            .saturating_add(config.request_timeout_ms),
    );
    let attempt_timeout_ms = delay_ms
        .saturating_add((MAX_RETRY_AFTER_S as u64).saturating_mul(1_000))
        .saturating_add(requests_ms)
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
    let exchange = SchemaExchange::new(&config.capabilities);
    check_request_budget(
        &base_request,
        &exchange,
        &transport,
        config.control_plane.transport,
    )?;
    let metrics = config.metrics.clone();

    let ctx = Arc::new(SyncContext {
        base_request,
        delay_ms,
        transport,
        exchange: Mutex::new(exchange),
        retry_hold_until: Mutex::new(None),
        startup_jitter_applied: AtomicBool::new(false),
        metrics,
        uptime,
    });

    let task: TaskRef = TaskFn::arc(move |cancel: TaskContext| {
        let ctx = Arc::clone(&ctx);
        async move { ctx.attempt(&cancel).await }
    });
    let manifest =
        TaskManifest::new(SLOT, spec).map_err(|e| DiscoverError::SpecBuild(e.to_string()))?;
    log_workload_types(&config.capabilities);
    Ok((manifest, task))
}

struct SyncContext {
    base_request: SyncRequest,
    delay_ms: u64,
    transport: TransportAdapter,
    exchange: Mutex<SchemaExchange>,
    retry_hold_until: Mutex<Option<Instant>>,
    startup_jitter_applied: AtomicBool,
    metrics: DiscoverMetricsHandle,
    uptime: Arc<dyn UptimeSource>,
}

/// A request that failed after it reached the transport.
struct Failed {
    error: DiscoverError,
    reason: DiscoverFailReason,
    duration_ms: u64,
}

impl SyncContext {
    /// Runs one attempt: the bare heartbeat, then follow-ups that carry the
    /// requested schemas while any are left to send.
    async fn attempt(&self, cancel: &TaskContext) -> Result<(), TaskError> {
        if !self.startup_jitter_applied.swap(true, Ordering::Relaxed) {
            let jitter = Duration::from_millis(startup_jitter_ms(self.delay_ms));
            debug!(
                event = "discovery.startup_jitter",
                jitter_ms = jitter.as_millis() as u64,
                "discovery startup delayed",
            );
            cancel
                .run_until_cancelled(tokio::time::sleep(jitter))
                .await?;
        }

        if let Some(wait) = self.retry_hold_wait() {
            debug!(
                event = "discovery.retry_hold",
                wait_s = wait.as_secs(),
                "discovery retry delayed"
            );
            cancel.run_until_cancelled(tokio::time::sleep(wait)).await?;
        }

        // The heartbeat never carries a schema, so a schema the control plane
        // or a proxy refuses, or a link too slow for it, never fails the
        // heartbeat. Its response restates what is requested.
        let mut sent = HashSet::new();
        let heartbeat = stamp_request(&self.base_request, self.uptime.as_ref())
            .map_err(TaskError::fatal_from)?;
        let now = Instant::now();
        let mut follow_up = match self.send(cancel, heartbeat, &[], 0).await? {
            Ok(response) => self.take_in(&[], &response, now, &mut sent),
            Err(failed) => return Err(heartbeat_failed(failed)),
        };

        for round in 1..=MAX_SCHEMA_FOLLOW_UPS {
            if !follow_up {
                break;
            }
            let mut request = match stamp_request(&self.base_request, self.uptime.as_ref()) {
                Ok(request) => request,
                Err(error) => {
                    follow_up_failed(
                        round,
                        &Failed {
                            reason: classify_failure(&error),
                            error,
                            duration_ms: 0,
                        },
                    );
                    return Ok(());
                }
            };
            let now = Instant::now();
            let attached = self.attach(&mut request, now, &sent);
            if attached.is_empty() {
                // No requested schema fits in a request even alone: no empty
                // follow-up.
                break;
            }
            follow_up = match self.send(cancel, request, &attached, round).await? {
                Ok(response) => self.take_in(&attached, &response, now, &mut sent),
                Err(failed) => {
                    follow_up_failed(round, &failed);
                    return Ok(());
                }
            };
        }
        Ok(())
    }

    /// Adds the sendable requested schemas that fit to a follow-up `request`.
    fn attach(
        &self,
        request: &mut SyncRequest,
        now: Instant,
        sent: &HashSet<SchemaFingerprint>,
    ) -> Vec<SchemaFingerprint> {
        self.exchange().attach(
            request,
            now,
            |request| self.transport.encoded_len(request),
            sent,
        )
    }

    /// Sends one request and records its metrics.
    ///
    /// A failure also applies any server hold it advises. The outer error is
    /// the cancellation of the attempt.
    async fn send(
        &self,
        cancel: &TaskContext,
        request: SyncRequest,
        attached: &[SchemaFingerprint],
        round: usize,
    ) -> Result<Result<SyncResponse, Failed>, TaskError> {
        let request_bytes = (!attached.is_empty()).then(|| self.transport.encoded_len(&request));
        debug!(
            event = "discovery.sync",
            stage = "started",
            round,
            "discovery sync started"
        );
        metrics::record_attempt(&self.metrics);
        let start = Instant::now();
        let result = cancel
            .run_until_cancelled(self.transport.sync(request))
            .await?;
        let duration_ms = start.elapsed().as_millis() as u64;
        match result {
            Ok(response) => {
                metrics::record_success(&self.metrics, duration_ms);
                self.clear_retry_hold();
                debug!(
                    event = "discovery.sync",
                    stage = "completed",
                    round,
                    duration_ms,
                    "discovery sync completed"
                );
                if let Some(request_bytes) = request_bytes {
                    info!(
                        event = "discovery.schemas_sent",
                        round,
                        count = attached.len(),
                        fingerprints = %join_fingerprints(attached),
                        request_bytes,
                        "workload schemas sent"
                    );
                }
                Ok(Ok(response))
            }
            Err(error) => {
                let reason = classify_failure(&error);
                metrics::record_failure(&self.metrics, duration_ms, reason);
                self.hold_if_advised(&error);
                Ok(Err(Failed {
                    error,
                    reason,
                    duration_ms,
                }))
            }
        }
    }

    /// Takes in an accepted response to a request that carried `attached`,
    /// logs what it reports, and returns whether a follow-up has schemas to
    /// carry.
    fn take_in(
        &self,
        attached: &[SchemaFingerprint],
        response: &SyncResponse,
        now: Instant,
        sent: &mut HashSet<SchemaFingerprint>,
    ) -> bool {
        let mut exchange = self.exchange();
        let observation = exchange.observe(attached, &response.want_schemas, now, sent);
        for fingerprint in &observation.still_requested {
            warn!(
                event = "discovery.schema_still_requested",
                fingerprint = %fingerprint,
                workload_types = %exchange.gvks(fingerprint),
                schema_bytes = exchange.schema_bytes(fingerprint),
                "control plane still requests a workload schema it was sent; resending at most every 5 minutes"
            );
        }
        if !observation.unknown.is_empty() {
            debug!(
                event = "discovery.schema_unknown_request",
                count = observation.unknown.len(),
                fingerprints = %sample_unknown(&observation.unknown),
                "control plane requested workload schemas this agent does not declare; ignored"
            );
        }
        observation.follow_up
    }

    fn exchange(&self) -> MutexGuard<'_, SchemaExchange> {
        self.exchange.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the server hold that a rejection advises, capped at one hour.
    fn hold_if_advised(&self, error: &DiscoverError) {
        if let DiscoverError::Rejected {
            retry_after_s: Some(s),
            ..
        } = error
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
            self.set_retry_hold(Duration::from_secs(clamped as u64));
            metrics::record_hold(&self.metrics, clamped as u64);
        }
    }

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

/// Logs a failed heartbeat and converts it into the attempt's error.
fn heartbeat_failed(failed: Failed) -> TaskError {
    let Failed {
        error,
        reason,
        duration_ms,
    } = failed;
    match error.retryability() {
        Retryability::Permanent => {
            warn!(
                event = "discovery.sync_failed",
                error_kind = reason.as_label(),
                retryable = false,
                duration_ms,
                "discovery sync failed"
            );
            TaskError::fatal_from(error)
        }
        Retryability::Retryable => {
            debug!(
                event = "discovery.sync_failed",
                error_kind = reason.as_label(),
                retryable = true,
                duration_ms,
                "discovery sync failed"
            );
            TaskError::fail_from(error)
        }
    }
}

/// Logs a failed follow-up. The heartbeat already succeeded, so the attempt
/// stays successful, and the next attempt sends what is still requested.
fn follow_up_failed(round: usize, failed: &Failed) {
    warn!(
        event = "discovery.schema_follow_up_failed",
        round,
        error_kind = failed.reason.as_label(),
        retryable = failed.error.retryability() == Retryability::Retryable,
        duration_ms = failed.duration_ms,
        error = %failed.error,
        "schema follow-up failed; the heartbeat succeeded and the next attempt sends what is still requested"
    );
}

/// Refuses a heartbeat that encodes over the request budget, and warns about
/// each declared schema that a follow-up cannot carry even alone: a follow-up
/// is the heartbeat's request plus its schemas.
///
/// The heartbeat is measured with the largest timestamp and uptime the wire
/// carries, so no later request is larger before its schemas.
fn check_request_budget(
    base: &SyncRequest,
    exchange: &SchemaExchange,
    transport: &TransportAdapter,
    transport_kind: DiscoveryTransport,
) -> Result<(), DiscoverError> {
    let widest = SyncRequest {
        ts: i64::MAX,
        uptime_seconds: i64::MAX,
        ..base.clone()
    };
    let bytes = transport.encoded_len(&widest);
    if bytes > SYNC_REQUEST_BUDGET {
        return Err(DiscoverError::InvalidConfig(format!(
            "the discovery heartbeat without schemas encodes to {bytes} bytes for {transport_kind:?} discovery, \
             over the request budget of {SYNC_REQUEST_BUDGET} bytes \
             ({MAX_SYNC_REQUEST_BYTES} minus {SYNC_REQUEST_HEADROOM_BYTES} of headroom); \
             declare fewer runners or workload types, or send less metadata"
        )));
    }
    for fingerprint in exchange.unsendable(&widest, |request| transport.encoded_len(request)) {
        warn!(
            event = "discovery.schema_over_request_budget",
            fingerprint = %fingerprint,
            workload_types = %exchange.gvks(&fingerprint),
            schema_bytes = exchange.schema_bytes(&fingerprint),
            budget_bytes = SYNC_REQUEST_BUDGET,
            "workload schema cannot fit in a discovery follow-up even alone; the control plane never receives it"
        );
    }
    Ok(())
}

/// Logs each declared `apiVersion/kind` with its schema fingerprint.
fn log_workload_types(capabilities: &AgentCapabilities) {
    let mut seen = HashSet::new();
    let types: Vec<String> = capabilities
        .runners()
        .iter()
        .flat_map(|runner| runner.workload_types())
        .filter(|workload| seen.insert((workload.meta(), workload.fingerprint())))
        .map(|workload| {
            format!(
                "{}/{}={}",
                workload.api_version(),
                workload.kind(),
                workload.fingerprint()
            )
        })
        .collect();
    info!(
        event = "discovery.workload_types",
        count = types.len(),
        schemas = capabilities.schemas().len(),
        types = %types.join(", "),
        "declared workload types"
    );
}

fn join_fingerprints(fingerprints: &[SchemaFingerprint]) -> String {
    fingerprints
        .iter()
        .map(SchemaFingerprint::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first unknown entries, each cut on a character boundary: they are
/// untrusted server text.
fn sample_unknown(unknown: &[String]) -> String {
    unknown
        .iter()
        .take(MAX_LOGGED_UNKNOWN)
        .map(|entry| {
            let mut end = entry.len().min(MAX_LOGGED_UNKNOWN_BYTES);
            while !entry.is_char_boundary(end) {
                end -= 1;
            }
            &entry[..end]
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the canonical metric label for a [`DiscoverError`].
fn classify_failure(err: &DiscoverError) -> metrics::DiscoverFailReason {
    match err {
        DiscoverError::InvalidConfig(_) | DiscoverError::SpecBuild(_) => {
            metrics::DiscoverFailReason::Other
        }
        DiscoverError::Rejected { .. } => metrics::DiscoverFailReason::RejectedClient,
        DiscoverError::AuthFailed { .. } => metrics::DiscoverFailReason::Auth,
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
        DiscoverError::HttpStatus { code, .. } => {
            if *code >= 500 {
                metrics::DiscoverFailReason::RejectedServer
            } else {
                metrics::DiscoverFailReason::RejectedClient
            }
        }
        DiscoverError::InvalidResponse(_) => metrics::DiscoverFailReason::Parse,
        DiscoverError::GrpcTransport(_) => metrics::DiscoverFailReason::Connect,
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
        schemas: Vec::new(),
    }
}

/// Encodes the capability snapshot.
///
/// Runners keep their registration order and each runner its own canonically
/// ordered types, so two runners may list the same GVK. Each type carries the
/// fingerprint of its schema; the schema itself travels only when requested.
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
                        schema_fingerprint: workload.fingerprint().as_str().to_owned(),
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
    use solti_model::{Labels, RunnerCapability};

    use std::sync::atomic::AtomicUsize;

    use crate::tasks::schemas::fixtures::{small_schema, workload};

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
                    workload(
                        "tasks.example.io/v1",
                        "DatabaseBackup",
                        small_schema("backup"),
                    ),
                    workload("exec.solti.io/v1", "Subprocess", small_schema("subprocess")),
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
                ("exec.solti.io/v1", "Subprocess"),
                ("tasks.example.io/v1", "DatabaseBackup"),
            ],
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
