use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use taskvisor::{TaskError, TaskFn, TaskRef};
use tno_core::{agent_id, arch, os_info, platform, uptime_seconds};
use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, RunnerLabels,
    TaskKind,
};

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;
use crate::{SyncRequest, SyncResponse, discover_service_client::DiscoverServiceClient};

const SLOT: &str = "tno-discover-sync";

pub fn sync(config: DiscoverConfig) -> (TaskRef, CreateSpec) {
    let delay_ms = config.delay_ms;

    let backoff = BackoffStrategy {
        jitter: JitterStrategy::Equal,
        first_ms: delay_ms / 2,
        max_ms: delay_ms * 3,
        factor: 2.0,
    };
    let spec = CreateSpec {
        slot: SLOT.to_string(),
        timeout_ms: delay_ms,
        restart: RestartStrategy::periodic(delay_ms),
        backoff,
        admission: AdmissionStrategy::Replace,
        kind: TaskKind::None,
        labels: RunnerLabels::default(),
    };

    let base_request = build_base_request(&config);
    let http_client = reqwest::Client::new();
    let ctx = Arc::new(SyncContext {
        base_request,
        http_client,
        config,
    });

    let task: TaskRef = TaskFn::arc(SLOT, move |cancel: CancellationToken| {
        let ctx = Arc::clone(&ctx);

        async move {
            if cancel.is_cancelled() {
                return Err(TaskError::Canceled);
            }
            debug!("sending sync request to control plane");

            match invoke_sync(&ctx).await {
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
            }
        }
    });
    (task, spec)
}

struct SyncContext {
    config: DiscoverConfig,
    base_request: SyncRequest,
    http_client: reqwest::Client,
}

async fn invoke_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    match ctx.config.transport {
        DiscoveryTransport::Grpc => invoke_grpc_sync(ctx).await,
        DiscoveryTransport::Http => invoke_http_sync(ctx).await,
    }
}

async fn invoke_grpc_sync(ctx: &SyncContext) -> Result<(), DiscoverError> {
    let mut client =
        DiscoverServiceClient::connect(ctx.config.control_plane_endpoint.clone()).await?;
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

    let body = response.text().await?;
    let sync_response: SyncResponse = serde_json::from_str(&body).map_err(|e| {
        DiscoverError::InvalidResponse(format!("failed to parse response: {}, body: {}", e, body))
    })?;

    validate_response(sync_response)
}

fn build_base_request(cfg: &DiscoverConfig) -> SyncRequest {
    SyncRequest {
        id: agent_id().to_string(),
        name: cfg.name.clone(),
        endpoint: cfg.agent_endpoint.clone(),
        platform: platform().to_string(),
        arch: arch().to_string(),
        os: os_info(),
        metadata: cfg.metadata.clone(),
        ts: 0,
        uptime_seconds: 0,
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
