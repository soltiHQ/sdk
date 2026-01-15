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
        jitter: JitterStrategy::None,
        first_ms: delay_ms / 2,
        max_ms: delay_ms / 2,
        factor: 1.0,
    };
    let spec = CreateSpec {
        slot: SLOT.to_string(),
        timeout_ms: 15_000,
        restart: RestartStrategy::periodic(delay_ms),
        backoff,
        admission: AdmissionStrategy::Replace,
        kind: TaskKind::None,
        labels: RunnerLabels::default(),
    };
    let config = Arc::new(config);

    let task: TaskRef = TaskFn::arc(SLOT, move |ctx: CancellationToken| {
        let config = Arc::clone(&config);

        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }
            debug!("sending sync request to control plane");

            match invoke_sync(&config).await {
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

async fn invoke_sync(cfg: &DiscoverConfig) -> Result<(), DiscoverError> {
    match cfg.transport {
        DiscoveryTransport::Grpc => invoke_grpc_sync(cfg).await,
        DiscoveryTransport::Http => invoke_http_sync(cfg).await,
    }
}

async fn invoke_grpc_sync(cfg: &DiscoverConfig) -> Result<(), DiscoverError> {
    let mut client = DiscoverServiceClient::connect(cfg.endpoint.clone()).await?;
    let request = tonic::Request::new(build_sync_request(cfg));
    let response = client.sync(request).await?.into_inner();

    validate_response(response)
}

async fn invoke_http_sync(cfg: &DiscoverConfig) -> Result<(), DiscoverError> {
    let client = reqwest::Client::new();
    let request = build_sync_request(cfg);

    let response = client
        .post(format!("{}/v1/sync", cfg.endpoint))
        .json(&request)
        .send()
        .await?;

    let body = response.text().await?;
    let sync_response: SyncResponse = serde_json::from_str(&body)
        .map_err(|e| DiscoverError::InvalidResponse(format!("failed to parse response: {}, body: {}", e, body)))?;

    validate_response(sync_response)
}

fn build_sync_request(cfg: &DiscoverConfig) -> SyncRequest {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs() as i64;

    SyncRequest {
        uptime_seconds: uptime_seconds() as i64,
        platform: platform().to_string(),
        metadata: cfg.metadata.clone(),
        endpoint: cfg.endpoint.clone(),
        id: agent_id().to_string(),
        arch: arch().to_string(),
        name: cfg.name.clone(),
        os: os_info(),
        ts: now,
    }
}

fn validate_response(response: SyncResponse) -> Result<(), DiscoverError> {
    if !response.success {
        return Err(DiscoverError::Rejected(response.message));
    }
    Ok(())
}
