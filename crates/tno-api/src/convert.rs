use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, TaskEnv, Flag, JitterStrategy, RestartStrategy,
    RunnerLabels, TaskInfo, TaskKind, TaskStatus,
};

use crate::error::ApiError;
use crate::proto;

// ============================================================================
// TaskStatus conversions
// ============================================================================

impl From<TaskStatus> for proto::TaskStatus {
    fn from(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Pending => proto::TaskStatus::Pending,
            TaskStatus::Running => proto::TaskStatus::Running,
            TaskStatus::Succeeded => proto::TaskStatus::Succeeded,
            TaskStatus::Failed => proto::TaskStatus::Failed,
            TaskStatus::Timeout => proto::TaskStatus::Timeout,
            TaskStatus::Canceled => proto::TaskStatus::Canceled,
            TaskStatus::Exhausted => proto::TaskStatus::Exhausted,
        }
    }
}

// ============================================================================
// TaskInfo conversions
// ============================================================================

impl From<TaskInfo> for proto::TaskInfo {
    fn from(info: TaskInfo) -> Self {
        use std::time::UNIX_EPOCH;

        let created_at = info
            .created_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let updated_at = info
            .updated_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        proto::TaskInfo {
            id: info.id.to_string(),
            slot: info.slot,
            status: proto::TaskStatus::from(info.status) as i32,
            attempt: info.attempt,
            created_at,
            updated_at,
            error: info.error,
        }
    }
}

// ============================================================================
// CreateSpec conversions (Proto → Domain)
// ============================================================================

impl TryFrom<proto::CreateSpec> for CreateSpec {
    type Error = ApiError;

    fn try_from(spec: proto::CreateSpec) -> Result<Self, Self::Error> {
        let kind = spec
            .kind
            .ok_or_else(|| ApiError::InvalidRequest("missing task kind".into()))?
            .kind // добавить .kind для unwrap oneof
            .ok_or_else(|| ApiError::InvalidRequest("missing task kind variant".into()))?;

        let task_kind = convert_task_kind(kind)?;

        let restart = convert_restart_strategy(
            proto::RestartStrategy::try_from(spec.restart)
                .map_err(|_| ApiError::InvalidRequest("invalid restart strategy".into()))?,
            spec.restart_interval_ms,
        )?;

        let backoff = spec
            .backoff
            .ok_or_else(|| ApiError::InvalidRequest("missing backoff strategy".into()))?;

        Ok(CreateSpec {
            slot: validate_slot(spec.slot)?,
            kind: task_kind,
            timeout_ms: validate_timeout(spec.timeout_ms)?,
            restart,
            backoff: convert_backoff_strategy(backoff)?,
            admission: convert_admission_strategy(
                proto::AdmissionStrategy::try_from(spec.admission)
                    .map_err(|_| ApiError::InvalidRequest("invalid admission strategy".into()))?,
            )?,
            labels: convert_labels(spec.labels),
        })
    }
}

fn convert_task_kind(kind: proto::task_kind::Kind) -> Result<TaskKind, ApiError> {
    match kind {
        proto::task_kind::Kind::Subprocess(sub) => {
            if sub.command.trim().is_empty() {
                return Err(ApiError::InvalidRequest(
                    "subprocess command is empty".into(),
                ));
            }

            Ok(TaskKind::Subprocess {
                command: sub.command,
                args: sub.args,
                env: convert_env(sub.env),
                cwd: sub.cwd.map(std::path::PathBuf::from),
                fail_on_non_zero: Flag::from(sub.fail_on_non_zero),
            })
        }
        proto::task_kind::Kind::Wasm(wasm) => {
            if wasm.module.trim().is_empty() {
                return Err(ApiError::InvalidRequest("wasm module path is empty".into()));
            }

            Ok(TaskKind::Wasm {
                module: std::path::PathBuf::from(wasm.module),
                args: wasm.args,
                env: convert_env(wasm.env),
            })
        }
        proto::task_kind::Kind::Container(cont) => {
            if cont.image.trim().is_empty() {
                return Err(ApiError::InvalidRequest("container image is empty".into()));
            }

            Ok(TaskKind::Container {
                image: cont.image,
                command: if cont.command.is_empty() {
                    None
                } else {
                    Some(cont.command)
                },
                args: cont.args,
                env: convert_env(cont.env),
            })
        }
    }
}

fn convert_env(kvs: Vec<proto::KeyValue>) -> TaskEnv {
    let mut env = TaskEnv::new();
    for kv in kvs {
        env.push(kv.key, kv.value);
    }
    env
}

fn convert_restart_strategy(
    strategy: proto::RestartStrategy,
    interval_ms: Option<u64>,
) -> Result<RestartStrategy, ApiError> {
    match strategy {
        proto::RestartStrategy::Never => Ok(RestartStrategy::Never),
        proto::RestartStrategy::OnFailure => Ok(RestartStrategy::OnFailure),
        proto::RestartStrategy::Always => Ok(RestartStrategy::Always { interval_ms }),
        proto::RestartStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "restart strategy not specified".into(),
        )),
    }
}

fn convert_backoff_strategy(backoff: proto::BackoffStrategy) -> Result<BackoffStrategy, ApiError> {
    let jitter = proto::JitterStrategy::try_from(backoff.jitter)
        .map_err(|_| ApiError::InvalidRequest("invalid jitter strategy".into()))?;

    let jitter = match jitter {
        proto::JitterStrategy::None => JitterStrategy::None,
        proto::JitterStrategy::Full => JitterStrategy::Full,
        proto::JitterStrategy::Equal => JitterStrategy::Equal,
        proto::JitterStrategy::Decorrelated => JitterStrategy::Decorrelated,
        proto::JitterStrategy::Unspecified => {
            return Err(ApiError::InvalidRequest(
                "jitter strategy not specified".into(),
            ));
        }
    };

    if backoff.first_ms == 0 {
        return Err(ApiError::InvalidRequest(
            "backoff first_ms cannot be zero".into(),
        ));
    }
    if backoff.max_ms == 0 {
        return Err(ApiError::InvalidRequest(
            "backoff max_ms cannot be zero".into(),
        ));
    }
    if backoff.factor <= 0.0 {
        return Err(ApiError::InvalidRequest(
            "backoff factor must be positive".into(),
        ));
    }

    Ok(BackoffStrategy {
        jitter,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

fn convert_admission_strategy(
    strategy: proto::AdmissionStrategy,
) -> Result<AdmissionStrategy, ApiError> {
    match strategy {
        proto::AdmissionStrategy::DropIfRunning => Ok(AdmissionStrategy::DropIfRunning),
        proto::AdmissionStrategy::Replace => Ok(AdmissionStrategy::Replace),
        proto::AdmissionStrategy::Queue => Ok(AdmissionStrategy::Queue),
        proto::AdmissionStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "admission strategy not specified".into(),
        )),
    }
}

fn convert_labels(map: std::collections::HashMap<String, String>) -> RunnerLabels {
    let mut labels = RunnerLabels::new();
    for (k, v) in map {
        labels.insert(k, v);
    }
    labels
}

fn validate_slot(slot: String) -> Result<String, ApiError> {
    if slot.trim().is_empty() {
        return Err(ApiError::InvalidRequest("slot cannot be empty".into()));
    }
    Ok(slot)
}

fn validate_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    if timeout_ms == 0 {
        return Err(ApiError::InvalidRequest("timeout_ms cannot be zero".into()));
    }
    Ok(timeout_ms)
}
