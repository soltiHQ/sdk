use std::time::{SystemTime, UNIX_EPOCH};

use taskvisor::{TaskError, TaskFn, TaskRef};
use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, RunnerLabels,
    TaskKind,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::config::AutodiscoveryConfig;
use crate::proto_autodiscovery::{
    HeartbeatRequest, lighthouse_discovery_client::LighthouseDiscoveryClient,
};

const HEARTBEAT_SLOT: &str = "tno-api-autodiscovery-heartbeat";

pub fn build_heartbeat_task(config: AutodiscoveryConfig) -> (TaskRef, CreateSpec) {
    let config_for_task = config.clone();
    let config_for_spec = config.clone();

    let task: TaskRef = TaskFn::arc(HEARTBEAT_SLOT, move |ctx: CancellationToken| {
        let cfg = config_for_task.clone();
        async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }

            debug!("sending heartbeat to lighthouse");

            match send_heartbeat(&cfg).await {
                Ok(()) => {
                    debug!("heartbeat sent successfully");
                    Ok(())
                }
                Err(e) => {
                    warn!("heartbeat failed: {}", e);
                    Err(TaskError::Fail {
                        reason: format!("heartbeat failed: {}", e),
                    })
                }
            }
        }
    });

    let backoff = BackoffStrategy {
        jitter: JitterStrategy::None,
        first_ms: config_for_spec.heartbeat_timeout_ms,
        max_ms: config_for_spec.heartbeat_timeout_ms * 2,
        factor: 1.0,
    };
    let spec = CreateSpec {
        slot: HEARTBEAT_SLOT.to_string(),
        timeout_ms: config_for_spec.heartbeat_timeout_ms,
        restart: RestartStrategy::periodic(config_for_spec.heartbeat_interval_ms),
        backoff,
        admission: AdmissionStrategy::Replace,
        kind: TaskKind::None,
        labels: RunnerLabels::default(),
    };
    (task, spec)
}

async fn send_heartbeat(config: &AutodiscoveryConfig) -> Result<(), String> {
    let endpoint = config.lighthouse_endpoint.clone();

    let mut client = LighthouseDiscoveryClient::connect(endpoint)
        .await
        .map_err(|e| format!("failed to connect to lighthouse: {}", e))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let request = tonic::Request::new(HeartbeatRequest {
        agent_id: config.agent_id.clone(),
        endpoint: config.agent_endpoint.clone(),
        timestamp,
        uptime_seconds: 0,
        capabilities: vec![],
    });

    let response = client
        .heartbeat(request)
        .await
        .map_err(|e| format!("heartbeat RPC failed: {}", e))?;
    let resp = response.into_inner();
    if !resp.acknowledged {
        return Err("lighthouse did not acknowledge heartbeat".into());
    }
    Ok(())
}
