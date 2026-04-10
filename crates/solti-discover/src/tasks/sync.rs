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
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use solti_core::uptime_seconds;
use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskError, TaskFn, TaskRef};

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;
use crate::proto::{
    SyncRequest, SyncResponse, discover_service_client::DiscoverServiceClient,
};

const SLOT: &str = "solti-discover-sync";

/// Build a heartbeat task and its spec from discovery config.
///
/// Returns `(TaskRef, TaskSpec)` ready for `SupervisorApi::submit_with_task`.
///
/// ## Also
///
/// - [`DiscoverConfig`](crate::DiscoverConfig) controls endpoint, transport, interval.
/// - [`DiscoverError`](crate::DiscoverError) failure modes surfaced via `TaskError::Fail`.
pub fn sync(config: DiscoverConfig) -> (TaskRef, TaskSpec) {
    let delay_ms = config.delay_ms;

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: delay_ms / 2,
        max_ms: delay_ms * 3,
        factor: 2.0,
    };
    let spec = TaskSpec::builder(SLOT, TaskKind::Embedded, delay_ms)
        .restart(RestartPolicy::periodic(delay_ms))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .expect("discover sync spec must be valid");

    let base_request = build_base_request(&config);
    let http_client = reqwest::Client::new();
    let ctx = Arc::new(SyncContext {
        base_request,
        http_client,
        config,
        grpc_client: tokio::sync::OnceCell::new(),
    });

    let task: TaskRef = TaskFn::arc(SLOT, move |cancel: CancellationToken| {
        let ctx = Arc::clone(&ctx);

        async move {
            debug!("sending sync request to control plane");

            tokio::select! {
                _ = cancel.cancelled() => Err(TaskError::Canceled),
                result = invoke_sync(&ctx) => match result {
                    Ok(()) => {
                        debug!("sync completed successfully");
                        Ok(())
                    }
                    Err(e) => {
                        warn!("sync failed: {}", e);
                        Err(TaskError::Fail {
                            reason: format!("sync failed: {}", e),
                        })
                    }
                },
            }
        }
    });
    (task, spec)
}

struct SyncContext {
    config: DiscoverConfig,
    base_request: SyncRequest,
    http_client: reqwest::Client,
    grpc_client: tokio::sync::OnceCell<DiscoverServiceClient<tonic::transport::Channel>>,
}

async fn invoke_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    match ctx.config.transport {
        DiscoveryTransport::Grpc => invoke_grpc_sync(ctx).await,
        DiscoveryTransport::Http => invoke_http_sync(ctx).await,
    }
}

async fn invoke_grpc_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let client = ctx
        .grpc_client
        .get_or_try_init(|| async {
            DiscoverServiceClient::connect(ctx.config.control_plane_endpoint.clone()).await
        })
        .await?;

    let mut client = client.clone();
    let request = tonic::Request::new(stamp_request(&ctx.base_request));
    let response = client.sync(request).await?.into_inner();

    validate_response(response)
}

async fn invoke_http_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let request = stamp_request(&ctx.base_request);

    let response = ctx
        .http_client
        .post(format!(
            "{}/api/v1/discovery/sync",
            ctx.config.control_plane_endpoint
        ))
        .json(&request)
        .send()
        .await?;

    let response = response.error_for_status()?;
    let body = response.text().await?;
    let sync_response: SyncResponse = serde_json::from_str(&body).map_err(|e| {
        DiscoverError::InvalidResponse(format!("failed to parse response: {}, body: {}", e, body))
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

/// Get OS distribution info (Linux only, the best effort).
fn os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                    return name.trim_matches('"').to_string();
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
        heartbeat_interval_s: (cfg.delay_ms / 1000) as i32,
        capabilities: vec![],
    }
}

fn stamp_request(base: &SyncRequest) -> SyncRequest {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs() as i64;

    SyncRequest {
        ts: now,
        uptime_seconds: uptime_seconds() as i64,
        ..base.clone()
    }
}

fn validate_response(response: SyncResponse) -> Result<(), DiscoverError> {
    if !response.success {
        return Err(DiscoverError::Rejected);
    }
    Ok(())
}
