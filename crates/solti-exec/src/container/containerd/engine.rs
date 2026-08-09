//! Native containerd 2.x lifecycle adapter.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::Duration,
};

use async_trait::async_trait;
use containerd_client::{
    Client,
    services::v1::{
        Container, CreateContainerRequest, CreateTaskRequest, DeleteContainerRequest,
        DeleteTaskRequest, GetContainerRequest, GetRequest, KillRequest, PluginInfoRequest,
        PluginsRequest, StartRequest, WaitRequest, WaitResponse,
        container::Runtime,
        snapshots::{
            MountsRequest, PrepareSnapshotRequest, RemoveSnapshotRequest, StatSnapshotRequest,
        },
    },
    tonic::{
        Code, GrpcMethod, Response, Status,
        client::Grpc,
        codegen::http::uri::PathAndQuery,
        metadata::{Ascii, MetadataValue},
    },
};
use prost::Message;
use prost_types::Any;
use tokio::{sync::oneshot, task::JoinHandle};
use tracing::warn;

use super::{
    ContainerPlatform, ContainerdConfig,
    config::{normalize_architecture, normalize_os},
    image::{self, ImageResolveRequest},
    io::AttemptIo,
};
use crate::container::{
    ContainerAttempt, ContainerEngine, ContainerEngineError, ContainerEngineInfo,
    ContainerErrorClass, ContainerExitStatus, ContainerOutput, ContainerRequest,
};

const OCI_SPEC_TYPE_URL: &str = "types.containerd.io/opencontainers/runtime-spec/1/Spec";
const CONTAINERD_MAJOR_VERSION: u64 = 2;
const CLEANUP_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
const CLEANUP_BACKOFF_MAX: Duration = Duration::from_secs(2);
const RUNTIME_PLUGIN_ID: &str = "task";
const RUNTIME_PLUGIN_TYPE: &str = "io.containerd.runtime.v2";
const RUNTIME_INFO_TYPE: &str = "containerd.types.RuntimeInfo";
const SNAPSHOTTER_PLUGIN_TYPE: &str = "io.containerd.snapshotter.v1";
const SIGKILL: u32 = 9;
const SESSION_BYTES: usize = 16;
const WAIT_METHOD: &str = "/containerd.services.tasks.v1.Tasks/Wait";
const WAIT_SERVICE: &str = "containerd.services.tasks.v1.Tasks";

const LABEL_ATTEMPT: &str = "solti.io/attempt";
const LABEL_GENERATION: &str = "solti.io/generation";
const LABEL_MANAGED_BY: &str = "solti.io/managed-by";
const LABEL_RESOURCE_ID: &str = "solti.io/resource-id";
const LABEL_SESSION: &str = "solti.io/session";
const LABEL_TASK: &str = "solti.io/task";
const MANAGED_BY: &str = "solti-exec";

type WaitRpcResult = Result<Response<WaitResponse>, Status>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ownership {
    Absent,
    Foreign,
    Owned,
    Uncertain,
}

/// Native adapter for a configured containerd 2.x endpoint.
///
/// Construction validates the endpoint major version, snapshotter, platform, and OCI runtime.
/// The adapter never scans for sockets or starts a daemon.
pub struct ContainerdEngine {
    client: Arc<Client>,
    config: ContainerdConfig,
    namespace: MetadataValue<Ascii>,
    ids: ResourceIdGenerator,
}

impl ContainerdEngine {
    /// Connects to an explicit containerd 2.x Unix socket.
    ///
    /// The connection fails for another major version or an incompatible configured plugin.
    pub async fn connect(config: ContainerdConfig) -> Result<Self, ContainerEngineError> {
        let namespace = config.validate()?;
        let channel = tokio::time::timeout(
            config.control_timeout(),
            containerd_client::connect(config.socket()),
        )
        .await
        .map_err(|error| {
            ContainerEngineError::retryable_from("cannot connect to containerd", error)
        })?
        .map_err(|error| {
            ContainerEngineError::retryable_from("cannot connect to containerd", error)
        })?;
        let ids = ResourceIdGenerator::random()?;
        let engine = Self {
            client: Arc::new(Client::from(channel)),
            config,
            namespace,
            ids,
        };
        engine.probe().await?;
        Ok(engine)
    }

    /// Checks major version 2 and the configured snapshotter, platform, and OCI runtime.
    pub async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        let response = image::rpc_with_timeout(
            self.config.control_timeout(),
            "containerd version probe failed",
            self.client
                .version()
                .version(image::with_timeout((), self.config.control_timeout())),
        )
        .await?
        .into_inner();
        validate_version(&response.version)?;
        self.probe_snapshotter().await?;
        self.probe_runtime().await?;
        Ok(ContainerEngineInfo::new("containerd", response.version))
    }

    async fn probe_snapshotter(&self) -> Result<(), ContainerEngineError> {
        let response = image::rpc_with_timeout(
            self.config.control_timeout(),
            "containerd snapshotter introspection failed",
            self.client
                .introspection()
                .plugins(image::namespaced_with_timeout(
                    PluginsRequest {
                        filters: Vec::new(),
                    },
                    &self.namespace,
                    self.config.control_timeout(),
                )),
        )
        .await?
        .into_inner();
        let plugin = response
            .plugins
            .into_iter()
            .find(|plugin| {
                plugin.r#type == SNAPSHOTTER_PLUGIN_TYPE && plugin.id == self.config.snapshotter()
            })
            .ok_or_else(|| {
                ContainerEngineError::permanent(format!(
                    "containerd snapshotter {:?} is not registered",
                    self.config.snapshotter()
                ))
            })?;
        validate_plugin_ready("containerd snapshotter", plugin.init_err.as_ref())?;
        if !plugin.platforms.is_empty()
            && !plugin
                .platforms
                .iter()
                .any(|platform| platform_matches(platform, self.config.platform()))
        {
            return Err(ContainerEngineError::permanent(format!(
                "containerd snapshotter {:?} does not support platform {}/{}{}",
                self.config.snapshotter(),
                self.config.platform().os(),
                self.config.platform().architecture(),
                platform_variant_suffix(self.config.platform().variant())
            )));
        }
        Ok(())
    }

    async fn probe_runtime(&self) -> Result<(), ContainerEngineError> {
        let response = image::rpc_with_timeout(
            self.config.control_timeout(),
            "containerd runtime probe failed",
            self.client
                .introspection()
                .plugin_info(image::namespaced_with_timeout(
                    PluginInfoRequest {
                        r#type: RUNTIME_PLUGIN_TYPE.to_owned(),
                        id: RUNTIME_PLUGIN_ID.to_owned(),
                        options: Some(containerd_client::to_any(
                            &containerd_client::types::RuntimeRequest {
                                runtime_path: self.config.runtime().to_owned(),
                                options: None,
                            },
                        )),
                    },
                    &self.namespace,
                    self.config.control_timeout(),
                )),
        )
        .await?
        .into_inner();
        let plugin = response.plugin.ok_or_else(|| {
            ContainerEngineError::permanent("containerd runtime probe returned no plugin")
        })?;
        if plugin.r#type != RUNTIME_PLUGIN_TYPE || plugin.id != RUNTIME_PLUGIN_ID {
            return Err(ContainerEngineError::permanent(
                "containerd runtime probe returned another plugin",
            ));
        }
        validate_plugin_ready("containerd runtime task plugin", plugin.init_err.as_ref())?;
        if !plugin.platforms.is_empty()
            && !plugin
                .platforms
                .iter()
                .any(|platform| platform_matches(platform, self.config.platform()))
        {
            return Err(ContainerEngineError::permanent(format!(
                "containerd runtime task plugin does not support platform {}/{}{}",
                self.config.platform().os(),
                self.config.platform().architecture(),
                platform_variant_suffix(self.config.platform().variant())
            )));
        }
        let extra = response.extra.ok_or_else(|| {
            ContainerEngineError::permanent("containerd runtime probe returned no runtime info")
        })?;
        let runtime = decode_runtime_info(extra)?;
        if runtime.name.trim().is_empty() {
            return Err(ContainerEngineError::permanent(
                "containerd runtime probe returned an empty runtime name",
            ));
        }
        Ok(())
    }

    async fn create_owned_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<ContainerdAttempt, ContainerEngineError> {
        let resource_id = self.ids.next()?;
        let labels = attempt_labels(&request, &resource_id, self.ids.session());
        let resolved = image::resolve(
            self.client.as_ref(),
            &self.namespace,
            ImageResolveRequest {
                reference: request.image(),
                platform: self.config.platform(),
                snapshotter: self.config.snapshotter(),
                registry_host_dir: self.config.registry_host_dir(),
                control_timeout: self.config.control_timeout(),
                transfer_timeout: self.config.transfer_timeout(),
            },
        )
        .await?;
        let spec = super::spec::build(&request, &resolved, &self.config, &resource_id)?;
        let spec = serde_json::to_vec(&spec).map_err(|error| {
            ContainerEngineError::permanent_from("cannot encode OCI runtime specification", error)
        })?;

        let io = AttemptIo::prepare(self.config.io_root(), &resource_id)
            .map_err(|error| io_error("cannot prepare containerd output pipes", error))?;
        let mut attempt = ContainerdAttempt::new(
            Arc::clone(&self.client),
            self.namespace.clone(),
            self.config.snapshotter().to_owned(),
            resource_id,
            labels,
            io,
            AttemptTimeouts {
                control: self.config.control_timeout(),
                cleanup: self.config.cleanup_timeout(),
            },
        );

        let create_result = attempt
            .create_resources(
                &resolved.reference,
                &resolved.chain_id,
                self.config.runtime(),
                spec,
            )
            .await;
        match create_result {
            Ok(()) => Ok(attempt),
            Err(creation) => match attempt.cleanup_owned_with_retry().await {
                Ok(()) => Err(creation),
                Err(rollback) => Err(ContainerEngineError::permanent_from(
                    "containerd attempt creation failed and rollback was incomplete",
                    CreationRollbackFailure { creation, rollback },
                )),
            },
        }
    }
}

fn decode_runtime_info(
    extra: Any,
) -> Result<containerd_client::types::RuntimeInfo, ContainerEngineError> {
    let type_name = extra
        .type_url
        .rsplit_once('/')
        .map_or(extra.type_url.as_str(), |(_, name)| name);
    if type_name != RUNTIME_INFO_TYPE {
        return Err(ContainerEngineError::permanent(format!(
            "containerd runtime probe returned type {:?}; expected {RUNTIME_INFO_TYPE:?}",
            extra.type_url
        )));
    }

    containerd_client::types::RuntimeInfo::decode(extra.value.as_slice()).map_err(|error| {
        ContainerEngineError::permanent_from(
            "containerd runtime probe returned invalid runtime info",
            error,
        )
    })
}

#[async_trait]
impl ContainerEngine for ContainerdEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        ContainerdEngine::probe(self).await
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.create_owned_attempt(request)
            .await
            .map(|attempt| Box::new(attempt) as Box<dyn ContainerAttempt>)
    }
}

struct ContainerdAttempt {
    client: Arc<Client>,
    namespace: MetadataValue<Ascii>,
    snapshotter: String,
    resource_id: String,
    labels: HashMap<String, String>,
    io: Option<AttemptIo>,
    snapshot: Ownership,
    container: Ownership,
    task: Ownership,
    wait: Option<JoinHandle<WaitRpcResult>>,
    exit_status: Option<ContainerExitStatus>,
    start_requested: bool,
    start_confirmed: bool,
    termination_sent: bool,
    termination_requires_wait: bool,
    control_timeout: Duration,
    cleanup_timeout: Duration,
}

#[derive(Clone, Copy)]
struct AttemptTimeouts {
    control: Duration,
    cleanup: Duration,
}

impl ContainerdAttempt {
    fn new(
        client: Arc<Client>,
        namespace: MetadataValue<Ascii>,
        snapshotter: String,
        resource_id: String,
        labels: HashMap<String, String>,
        io: AttemptIo,
        timeouts: AttemptTimeouts,
    ) -> Self {
        Self {
            client,
            namespace,
            snapshotter,
            resource_id,
            labels,
            io: Some(io),
            snapshot: Ownership::Absent,
            container: Ownership::Absent,
            task: Ownership::Absent,
            wait: None,
            exit_status: None,
            start_requested: false,
            start_confirmed: false,
            termination_sent: false,
            termination_requires_wait: false,
            control_timeout: timeouts.control,
            cleanup_timeout: timeouts.cleanup,
        }
    }

    async fn create_resources(
        &mut self,
        image_reference: &str,
        parent_snapshot: &str,
        runtime: &str,
        spec: Vec<u8>,
    ) -> Result<(), ContainerEngineError> {
        let snapshot = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd snapshot prepare failed",
            self.client
                .snapshots()
                .prepare(image::namespaced_with_timeout(
                    PrepareSnapshotRequest {
                        snapshotter: self.snapshotter.clone(),
                        key: self.resource_id.clone(),
                        parent: parent_snapshot.to_owned(),
                        labels: self.labels.clone(),
                    },
                    &self.namespace,
                    self.control_timeout,
                )),
        )
        .await;
        let mounts = match snapshot {
            Ok(response) => {
                self.snapshot = Ownership::Owned;
                response.into_inner().mounts
            }
            Err(status) if status.code() == Code::AlreadyExists => {
                self.snapshot = Ownership::Foreign;
                return Err(image::rpc_error(
                    "containerd snapshot prepare failed",
                    status,
                ));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.snapshot = Ownership::Uncertain;
                match self.confirm_snapshot_ownership(parent_snapshot).await {
                    Ok(true) => self.snapshot_mounts().await?,
                    Ok(false) => {
                        return Err(image::rpc_error(
                            "containerd snapshot prepare failed",
                            status,
                        ));
                    }
                    Err(readback) => {
                        warn!(
                            resource_id = %self.resource_id,
                            error = %readback,
                            "containerd snapshot create outcome could not be verified",
                        );
                        return Err(image::rpc_error(
                            "containerd snapshot prepare failed",
                            status,
                        ));
                    }
                }
            }
            Err(status) => {
                return Err(image::rpc_error(
                    "containerd snapshot prepare failed",
                    status,
                ));
            }
        };

        let container = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd container create failed",
            self.client
                .containers()
                .create(image::namespaced_with_timeout(
                    CreateContainerRequest {
                        container: Some(Container {
                            id: self.resource_id.clone(),
                            labels: self.labels.clone(),
                            image: image_reference.to_owned(),
                            runtime: Some(Runtime {
                                name: runtime.to_owned(),
                                options: None,
                            }),
                            spec: Some(Any {
                                type_url: OCI_SPEC_TYPE_URL.to_owned(),
                                value: spec,
                            }),
                            snapshotter: self.snapshotter.clone(),
                            snapshot_key: self.resource_id.clone(),
                            ..Default::default()
                        }),
                    },
                    &self.namespace,
                    self.control_timeout,
                )),
        )
        .await;
        match container {
            Ok(_) => self.container = Ownership::Owned,
            Err(status) if status.code() == Code::AlreadyExists => {
                self.container = Ownership::Foreign;
                return Err(image::rpc_error(
                    "containerd container create failed",
                    status,
                ));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.container = Ownership::Uncertain;
                match self.confirm_container_ownership().await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(image::rpc_error(
                            "containerd container create failed",
                            status,
                        ));
                    }
                    Err(readback) => {
                        warn!(
                            resource_id = %self.resource_id,
                            error = %readback,
                            "containerd container create outcome could not be verified",
                        );
                        return Err(image::rpc_error(
                            "containerd container create failed",
                            status,
                        ));
                    }
                }
            }
            Err(status) => {
                return Err(image::rpc_error(
                    "containerd container create failed",
                    status,
                ));
            }
        }

        let io = self.io.as_ref().ok_or_else(|| {
            ContainerEngineError::permanent("containerd attempt has no output pipes")
        })?;
        let stdout = io.stdout_path().to_str().ok_or_else(|| {
            ContainerEngineError::permanent("containerd stdout path is not valid UTF-8")
        })?;
        let stderr = io.stderr_path().to_str().ok_or_else(|| {
            ContainerEngineError::permanent("containerd stderr path is not valid UTF-8")
        })?;
        let task = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd task create failed",
            self.client.tasks().create(image::namespaced_with_timeout(
                CreateTaskRequest {
                    container_id: self.resource_id.clone(),
                    rootfs: mounts,
                    stdin: String::new(),
                    stdout: stdout.to_owned(),
                    stderr: stderr.to_owned(),
                    terminal: false,
                    checkpoint: None,
                    options: None,
                    runtime_path: String::new(),
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await;
        match task {
            Ok(_) => self.task = Ownership::Owned,
            Err(status) if status.code() == Code::AlreadyExists => {
                self.task = Ownership::Foreign;
                return Err(image::rpc_error("containerd task create failed", status));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.task = Ownership::Uncertain;
                match self.confirm_task_ownership().await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(image::rpc_error("containerd task create failed", status));
                    }
                    Err(readback) => {
                        warn!(
                            resource_id = %self.resource_id,
                            error = %readback,
                            "containerd task create outcome could not be verified",
                        );
                        return Err(image::rpc_error("containerd task create failed", status));
                    }
                }
            }
            Err(status) => {
                return Err(image::rpc_error("containerd task create failed", status));
            }
        }

        self.io
            .as_mut()
            .expect("attempt I/O exists until cleanup")
            .activate()
            .map_err(|error| io_error("cannot activate containerd output pipes", error))?;
        self.wait = Some(
            arm_wait(
                Arc::clone(&self.client),
                self.namespace.clone(),
                self.resource_id.clone(),
                self.control_timeout,
            )
            .await?,
        );
        Ok(())
    }

    async fn confirm_snapshot_ownership(
        &mut self,
        expected_parent: &str,
    ) -> Result<bool, ContainerEngineError> {
        self.confirm_snapshot_ownership_with_parent(Some(expected_parent))
            .await
    }

    async fn confirm_snapshot_ownership_for_cleanup(
        &mut self,
    ) -> Result<bool, ContainerEngineError> {
        self.confirm_snapshot_ownership_with_parent(None).await
    }

    async fn confirm_snapshot_ownership_with_parent(
        &mut self,
        expected_parent: Option<&str>,
    ) -> Result<bool, ContainerEngineError> {
        if self.snapshot == Ownership::Owned {
            return Ok(true);
        }
        if matches!(self.snapshot, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        let response = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd snapshot ownership read-back failed",
            self.client.snapshots().stat(image::namespaced_with_timeout(
                StatSnapshotRequest {
                    snapshotter: self.snapshotter.clone(),
                    key: self.resource_id.clone(),
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await;
        let info = match response {
            Ok(response) => response.into_inner().info.ok_or_else(|| {
                ContainerEngineError::permanent(
                    "containerd snapshot read-back returned no metadata",
                )
            })?,
            Err(status) if status.code() == Code::NotFound => {
                self.snapshot =
                    ownership_after_read_back(self.snapshot, OwnershipReadBack::Missing);
                return Ok(false);
            }
            Err(status) => {
                self.snapshot =
                    ownership_after_read_back(self.snapshot, OwnershipReadBack::Unavailable);
                return Err(image::rpc_error(
                    "containerd snapshot ownership read-back failed",
                    status,
                ));
            }
        };
        let matches = snapshot_identity_matches(
            &info.name,
            &info.parent,
            &info.labels,
            &self.resource_id,
            expected_parent,
            &self.labels,
        );
        self.snapshot = ownership_after_read_back(
            self.snapshot,
            if matches {
                OwnershipReadBack::Matching
            } else {
                OwnershipReadBack::Mismatched
            },
        );
        if !matches {
            return Err(ContainerEngineError::permanent(
                "containerd snapshot resource ID is owned by another resource",
            ));
        }
        Ok(true)
    }

    async fn snapshot_mounts(
        &mut self,
    ) -> Result<Vec<containerd_client::types::Mount>, ContainerEngineError> {
        image::rpc_with_timeout(
            self.control_timeout,
            "containerd snapshot mounts lookup failed",
            self.client
                .snapshots()
                .mounts(image::namespaced_with_timeout(
                    MountsRequest {
                        snapshotter: self.snapshotter.clone(),
                        key: self.resource_id.clone(),
                    },
                    &self.namespace,
                    self.control_timeout,
                )),
        )
        .await
        .map(|response| response.into_inner().mounts)
    }

    async fn confirm_container_ownership(&mut self) -> Result<bool, ContainerEngineError> {
        if self.container == Ownership::Owned {
            return Ok(true);
        }
        if matches!(self.container, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        let response = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd container ownership read-back failed",
            self.client.containers().get(image::namespaced_with_timeout(
                GetContainerRequest {
                    id: self.resource_id.clone(),
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await;
        let container = match response {
            Ok(response) => response.into_inner().container.ok_or_else(|| {
                ContainerEngineError::permanent(
                    "containerd container read-back returned no metadata",
                )
            })?,
            Err(status) if status.code() == Code::NotFound => {
                self.container =
                    ownership_after_read_back(self.container, OwnershipReadBack::Missing);
                return Ok(false);
            }
            Err(status) => {
                self.container =
                    ownership_after_read_back(self.container, OwnershipReadBack::Unavailable);
                return Err(image::rpc_error(
                    "containerd container ownership read-back failed",
                    status,
                ));
            }
        };
        let matches = container_identity_matches(
            &container.id,
            &container.snapshotter,
            &container.snapshot_key,
            &container.labels,
            &self.resource_id,
            &self.snapshotter,
            &self.labels,
        );
        self.container = ownership_after_read_back(
            self.container,
            if matches {
                OwnershipReadBack::Matching
            } else {
                OwnershipReadBack::Mismatched
            },
        );
        if !matches {
            return Err(ContainerEngineError::permanent(
                "containerd container resource ID is owned by another resource",
            ));
        }
        Ok(true)
    }

    async fn confirm_task_ownership(&mut self) -> Result<bool, ContainerEngineError> {
        if self.task == Ownership::Owned {
            return Ok(true);
        }
        if matches!(self.task, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        if self.container != Ownership::Owned {
            return Err(ContainerEngineError::permanent(
                "cannot adopt a containerd task without its owned container metadata",
            ));
        }
        let response = image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd task ownership read-back failed",
            self.client.tasks().get(image::namespaced_with_timeout(
                GetRequest {
                    container_id: self.resource_id.clone(),
                    exec_id: String::new(),
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await;
        let process = match response {
            Ok(response) => response.into_inner().process.ok_or_else(|| {
                ContainerEngineError::permanent("containerd task read-back returned no process")
            })?,
            Err(status) if status.code() == Code::NotFound => {
                self.task = ownership_after_read_back(self.task, OwnershipReadBack::Missing);
                return Ok(false);
            }
            Err(status) => {
                self.task = ownership_after_read_back(self.task, OwnershipReadBack::Unavailable);
                return Err(image::rpc_error(
                    "containerd task ownership read-back failed",
                    status,
                ));
            }
        };
        let matches =
            task_identity_matches(self.container, &process.container_id, &self.resource_id);
        self.task = ownership_after_read_back(
            self.task,
            if matches {
                OwnershipReadBack::Matching
            } else {
                OwnershipReadBack::Mismatched
            },
        );
        if !matches {
            return Err(ContainerEngineError::permanent(
                "containerd task resource ID is owned by another resource",
            ));
        }
        Ok(true)
    }

    async fn ensure_wait_armed(&mut self) -> Result<(), ContainerEngineError> {
        if self.wait.is_none() && self.task == Ownership::Owned {
            self.wait = Some(
                arm_wait(
                    Arc::clone(&self.client),
                    self.namespace.clone(),
                    self.resource_id.clone(),
                    self.control_timeout,
                )
                .await?,
            );
        }
        Ok(())
    }

    async fn start_inner(&mut self) -> Result<(), ContainerEngineError> {
        if self.task != Ownership::Owned {
            return Err(ContainerEngineError::permanent(
                "containerd task is not available",
            ));
        }
        if self.start_confirmed {
            return Ok(());
        }
        if self.start_requested {
            return Err(ContainerEngineError::permanent(
                "containerd task start result is unknown",
            ));
        }

        self.start_requested = true;
        image::rpc_with_timeout(
            self.control_timeout,
            "containerd task start failed",
            self.client.tasks().start(image::namespaced_with_timeout(
                StartRequest {
                    container_id: self.resource_id.clone(),
                    exec_id: String::new(),
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await?;
        self.start_confirmed = true;
        Ok(())
    }

    async fn wait_inner(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        self.ensure_wait_armed().await?;
        let response = match self
            .wait
            .as_mut()
            .ok_or_else(|| {
                ContainerEngineError::permanent("containerd task wait is not available")
            })?
            .await
        {
            Ok(Ok(response)) => response.into_inner(),
            Ok(Err(status)) => {
                self.wait.take();
                if status.code() == Code::NotFound {
                    self.task = Ownership::Absent;
                }
                return Err(image::rpc_error("containerd task wait failed", status));
            }
            Err(error) => {
                self.wait.take();
                return Err(ContainerEngineError::retryable_from(
                    "containerd task wait worker failed",
                    error,
                ));
            }
        };
        self.wait.take();
        let code = i32::try_from(response.exit_status).map_err(|error| {
            ContainerEngineError::permanent_from(
                "containerd exit status exceeds the process exit-code range",
                error,
            )
        })?;
        let status = ContainerExitStatus::new(code);
        self.exit_status = Some(status);
        self.termination_requires_wait = false;
        Ok(status)
    }

    async fn terminate_inner(&mut self) -> Result<(), ContainerEngineError> {
        if self.task != Ownership::Owned || self.exit_status.is_some() || !self.start_requested {
            return Ok(());
        }
        if self.termination_sent {
            return Ok(());
        }

        if let Err(error) = self.ensure_wait_armed().await {
            warn!(
                resource_id = %self.resource_id,
                error = %error,
                "containerd wait could not be re-armed before termination",
            );
        }

        match image::raw_rpc_with_timeout(
            self.control_timeout,
            "containerd task termination failed",
            self.client.tasks().kill(image::namespaced_with_timeout(
                KillRequest {
                    container_id: self.resource_id.clone(),
                    exec_id: String::new(),
                    signal: SIGKILL,
                    all: true,
                },
                &self.namespace,
                self.control_timeout,
            )),
        )
        .await
        {
            Ok(_) => {
                self.termination_sent = true;
                self.termination_requires_wait = true;
                Ok(())
            }
            Err(status) if status.code() == Code::NotFound => {
                self.task = Ownership::Absent;
                self.abort_wait();
                Ok(())
            }
            Err(status) if status.code() == Code::FailedPrecondition => {
                self.termination_sent = true;
                self.termination_requires_wait = self.start_confirmed;
                if !self.termination_requires_wait {
                    self.abort_wait();
                }
                Ok(())
            }
            Err(status) => Err(image::rpc_error(
                "containerd task termination failed",
                status,
            )),
        }
    }

    async fn cleanup_owned(&mut self) -> Result<(), ContainerEngineError> {
        let mut failures = CleanupFailures::default();

        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .confirm_task
        {
            match self.confirm_task_ownership().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }

        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .delete_task
        {
            let mut task_can_be_deleted = true;
            let mut wait_error = None;
            if self.start_requested && self.exit_status.is_none() {
                if let Err(error) = self.terminate_inner().await {
                    failures.push(error);
                    task_can_be_deleted = false;
                } else if self.task == Ownership::Owned
                    && self.termination_requires_wait
                    && let Err(error) = self.wait_inner().await
                {
                    wait_error = Some(error);
                }
            } else if !self.start_requested {
                self.abort_wait();
            }

            if task_can_be_deleted
                && cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
                    .delete_task
            {
                match image::raw_rpc_with_timeout(
                    self.control_timeout,
                    "containerd task cleanup failed",
                    self.client.tasks().delete(image::namespaced_with_timeout(
                        DeleteTaskRequest {
                            container_id: self.resource_id.clone(),
                        },
                        &self.namespace,
                        self.control_timeout,
                    )),
                )
                .await
                {
                    Ok(_) => {
                        self.task = ownership_after_delete(self.task, OwnershipDelete::Removed);
                    }
                    Err(status) if status.code() == Code::NotFound => {
                        self.task = ownership_after_delete(self.task, OwnershipDelete::Missing);
                    }
                    Err(status) => {
                        self.task = ownership_after_delete(self.task, OwnershipDelete::Failed);
                        if let Some(error) = wait_error {
                            failures.push(error);
                        }
                        failures.push(image::rpc_error("containerd task cleanup failed", status));
                    }
                }
            }
            if self.task == Ownership::Absent {
                self.abort_wait();
            }
        }

        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .confirm_container
        {
            match self.confirm_container_ownership().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }
        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .delete_container
        {
            match image::raw_rpc_with_timeout(
                self.control_timeout,
                "containerd container cleanup failed",
                self.client
                    .containers()
                    .delete(image::namespaced_with_timeout(
                        DeleteContainerRequest {
                            id: self.resource_id.clone(),
                        },
                        &self.namespace,
                        self.control_timeout,
                    )),
            )
            .await
            {
                Ok(_) => {
                    self.container =
                        ownership_after_delete(self.container, OwnershipDelete::Removed);
                }
                Err(status) if status.code() == Code::NotFound => {
                    self.container =
                        ownership_after_delete(self.container, OwnershipDelete::Missing);
                }
                Err(status) => {
                    self.container =
                        ownership_after_delete(self.container, OwnershipDelete::Failed);
                    failures.push(image::rpc_error(
                        "containerd container cleanup failed",
                        status,
                    ));
                }
            }
        }

        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .confirm_snapshot
        {
            match self.confirm_snapshot_ownership_for_cleanup().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }
        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .delete_snapshot
        {
            match image::raw_rpc_with_timeout(
                self.control_timeout,
                "containerd snapshot cleanup failed",
                self.client
                    .snapshots()
                    .remove(image::namespaced_with_timeout(
                        RemoveSnapshotRequest {
                            snapshotter: self.snapshotter.clone(),
                            key: self.resource_id.clone(),
                        },
                        &self.namespace,
                        self.control_timeout,
                    )),
            )
            .await
            {
                Ok(_) => {
                    self.snapshot = ownership_after_delete(self.snapshot, OwnershipDelete::Removed);
                }
                Err(status) if status.code() == Code::NotFound => {
                    self.snapshot = ownership_after_delete(self.snapshot, OwnershipDelete::Missing);
                }
                Err(status) => {
                    self.snapshot = ownership_after_delete(self.snapshot, OwnershipDelete::Failed);
                    failures.push(image::rpc_error(
                        "containerd snapshot cleanup failed",
                        status,
                    ));
                }
            }
        }

        if cleanup_eligibility(self.task, self.container, self.snapshot, self.io.is_some())
            .cleanup_io
            && let Some(io) = self.io.as_mut()
        {
            match io.cleanup() {
                Ok(()) => self.io = None,
                Err(error) => {
                    failures.push(io_error("cannot clean up containerd output pipes", error))
                }
            }
        }

        failures.into_result()
    }

    async fn cleanup_owned_with_retry(&mut self) -> Result<(), ContainerEngineError> {
        let timeout = self.cleanup_timeout;
        let result =
            retry_cleanup(self, timeout, |attempt| Box::pin(attempt.cleanup_owned())).await;
        if result.is_err() {
            self.abort_wait();
        }
        result
    }

    fn abort_wait(&mut self) {
        if let Some(wait) = self.wait.take() {
            wait.abort();
        }
    }
}

impl Drop for ContainerdAttempt {
    fn drop(&mut self) {
        self.abort_wait();
    }
}

#[async_trait]
impl ContainerAttempt for ContainerdAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        self.io.as_mut().and_then(AttemptIo::take_stdout)
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        self.io.as_mut().and_then(AttemptIo::take_stderr)
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.start_inner().await
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.wait_inner().await
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.terminate_inner().await
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.cleanup_owned_with_retry().await
    }
}

fn cleanup_retry_exhausted(
    timeout: Duration,
    last_error: Option<ContainerEngineError>,
) -> ContainerEngineError {
    ContainerEngineError::retryable_from(
        "containerd attempt cleanup retry window exhausted",
        CleanupRetryExhausted {
            timeout,
            last_error,
        },
    )
}

type CleanupOperation<'a> =
    Pin<Box<dyn Future<Output = Result<(), ContainerEngineError>> + Send + 'a>>;

async fn retry_cleanup<T, F>(
    state: &mut T,
    timeout: Duration,
    mut operation: F,
) -> Result<(), ContainerEngineError>
where
    T: Send,
    F: for<'a> FnMut(&'a mut T) -> CleanupOperation<'a>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut backoff = CLEANUP_BACKOFF_INITIAL;
    let mut last_error = None;

    loop {
        match tokio::time::timeout_at(deadline, operation(state)).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) if error.class() == ContainerErrorClass::Permanent => {
                return Err(error);
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => return Err(cleanup_retry_exhausted(timeout, last_error)),
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(cleanup_retry_exhausted(timeout, last_error));
        }
        tokio::time::sleep_until((now + backoff).min(deadline)).await;
        backoff = backoff.saturating_mul(2).min(CLEANUP_BACKOFF_MAX);
    }
}

async fn arm_wait(
    client: Arc<Client>,
    namespace: MetadataValue<Ascii>,
    resource_id: String,
    control_timeout: Duration,
) -> Result<JoinHandle<WaitRpcResult>, ContainerEngineError> {
    let (armed_tx, armed_rx) = oneshot::channel();
    let wait = tokio::spawn(async move {
        let mut grpc = Grpc::new(client.channel());
        match tokio::time::timeout(control_timeout, grpc.ready()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let status =
                    Status::unknown(format!("containerd task service was not ready: {error}"));
                let _ = armed_tx.send(Err(status.clone()));
                return Err(status);
            }
            Err(_) => {
                let status = Status::deadline_exceeded(
                    "containerd task service readiness deadline exceeded",
                );
                let _ = armed_tx.send(Err(status.clone()));
                return Err(status);
            }
        }

        let mut request = image::namespaced(
            WaitRequest {
                container_id: resource_id,
                exec_id: String::new(),
            },
            &namespace,
        );
        request
            .extensions_mut()
            .insert(GrpcMethod::new(WAIT_SERVICE, "Wait"));
        let mut request = Box::pin(grpc.unary(
            request,
            PathAndQuery::from_static(WAIT_METHOD),
            tonic_prost::ProstCodec::default(),
        ));
        poll_wait_once(&mut request, armed_tx).await
    });

    match armed_rx.await {
        Ok(Ok(())) => Ok(wait),
        Ok(Err(status)) => {
            let _ = wait.await;
            Err(image::rpc_error("containerd task wait failed", status))
        }
        Err(error) => match wait.await {
            Ok(Ok(_)) => Err(ContainerEngineError::retryable_from(
                "containerd task wait arm signal was lost",
                error,
            )),
            Ok(Err(status)) => Err(image::rpc_error("containerd task wait failed", status)),
            Err(join) => Err(ContainerEngineError::retryable_from(
                "containerd task wait worker failed",
                join,
            )),
        },
    }
}

async fn poll_wait_once<F>(
    future: &mut Pin<Box<F>>,
    armed: oneshot::Sender<Result<(), Status>>,
) -> WaitRpcResult
where
    F: Future<Output = WaitRpcResult>,
{
    let mut armed = Some(armed);
    std::future::poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Ready(result) => {
            if let Some(armed) = armed.take() {
                let signal = result.as_ref().map(|_| ()).map_err(|status| status.clone());
                let _ = armed.send(signal);
            }
            Poll::Ready(result)
        }
        Poll::Pending => {
            if let Some(armed) = armed.take() {
                let _ = armed.send(Ok(()));
            }
            Poll::Pending
        }
    })
    .await
}

struct ResourceIdGenerator {
    session: String,
    sequence: AtomicU64,
}

impl ResourceIdGenerator {
    fn random() -> Result<Self, ContainerEngineError> {
        let mut session = [0_u8; SESSION_BYTES];
        getrandom::fill(&mut session).map_err(|error| {
            ContainerEngineError::retryable_from(
                "cannot create a containerd engine session identifier",
                error,
            )
        })?;
        Ok(Self::from_session(session))
    }

    fn from_session(session: [u8; SESSION_BYTES]) -> Self {
        Self {
            session: lower_hex(&session),
            sequence: AtomicU64::new(0),
        }
    }

    fn session(&self) -> &str {
        &self.session
    }

    fn next(&self) -> Result<String, ContainerEngineError> {
        let previous = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| ContainerEngineError::permanent("containerd resource ID exhausted"))?;
        let sequence = previous + 1;
        Ok(format!("solti-{}-{sequence:016x}", self.session))
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn attempt_labels(
    request: &ContainerRequest,
    resource_id: &str,
    session: &str,
) -> HashMap<String, String> {
    HashMap::from([
        (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
        (LABEL_SESSION.to_owned(), session.to_owned()),
        (LABEL_RESOURCE_ID.to_owned(), resource_id.to_owned()),
        (
            LABEL_TASK.to_owned(),
            request.task_name().as_str().to_owned(),
        ),
        (
            LABEL_GENERATION.to_owned(),
            request.generation().to_string(),
        ),
        (LABEL_ATTEMPT.to_owned(), request.attempt().to_string()),
    ])
}

fn has_ownership_labels(
    actual: &HashMap<String, String>,
    expected: &HashMap<String, String>,
) -> bool {
    expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipReadBack {
    Missing,
    Matching,
    Mismatched,
    Unavailable,
}

fn ownership_after_read_back(current: Ownership, outcome: OwnershipReadBack) -> Ownership {
    match outcome {
        OwnershipReadBack::Missing => Ownership::Absent,
        OwnershipReadBack::Matching => Ownership::Owned,
        OwnershipReadBack::Mismatched => Ownership::Foreign,
        OwnershipReadBack::Unavailable => current,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipDelete {
    Removed,
    Missing,
    Failed,
}

fn ownership_after_delete(current: Ownership, outcome: OwnershipDelete) -> Ownership {
    match outcome {
        OwnershipDelete::Removed | OwnershipDelete::Missing => Ownership::Absent,
        OwnershipDelete::Failed => current,
    }
}

fn snapshot_identity_matches(
    actual_name: &str,
    actual_parent: &str,
    actual_labels: &HashMap<String, String>,
    expected_resource_id: &str,
    expected_parent: Option<&str>,
    expected_labels: &HashMap<String, String>,
) -> bool {
    actual_name == expected_resource_id
        && expected_parent.is_none_or(|expected| actual_parent == expected)
        && has_ownership_labels(actual_labels, expected_labels)
}

fn container_identity_matches(
    actual_id: &str,
    actual_snapshotter: &str,
    actual_snapshot_key: &str,
    actual_labels: &HashMap<String, String>,
    expected_resource_id: &str,
    expected_snapshotter: &str,
    expected_labels: &HashMap<String, String>,
) -> bool {
    actual_id == expected_resource_id
        && actual_snapshotter == expected_snapshotter
        && actual_snapshot_key == expected_resource_id
        && has_ownership_labels(actual_labels, expected_labels)
}

fn task_identity_matches(
    container: Ownership,
    actual_container_id: &str,
    expected_resource_id: &str,
) -> bool {
    container == Ownership::Owned && actual_container_id == expected_resource_id
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CleanupEligibility {
    confirm_task: bool,
    delete_task: bool,
    confirm_container: bool,
    delete_container: bool,
    confirm_snapshot: bool,
    delete_snapshot: bool,
    cleanup_io: bool,
}

fn cleanup_eligibility(
    task: Ownership,
    container: Ownership,
    snapshot: Ownership,
    has_io: bool,
) -> CleanupEligibility {
    let task_absent = task == Ownership::Absent;
    let container_absent = container == Ownership::Absent;

    CleanupEligibility {
        confirm_task: task == Ownership::Uncertain,
        delete_task: task == Ownership::Owned,
        confirm_container: task_absent && container == Ownership::Uncertain,
        delete_container: task_absent && container == Ownership::Owned,
        confirm_snapshot: task_absent && container_absent && snapshot == Ownership::Uncertain,
        delete_snapshot: task_absent && container_absent && snapshot == Ownership::Owned,
        cleanup_io: has_io,
    }
}

fn ambiguous_create_status(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Cancelled
            | Code::Unknown
            | Code::DeadlineExceeded
            | Code::ResourceExhausted
            | Code::Aborted
            | Code::Internal
            | Code::Unavailable
            | Code::DataLoss
    )
}

fn validate_version(version: &str) -> Result<(), ContainerEngineError> {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let major = version
        .split_once('.')
        .map_or(version, |(major, _)| major)
        .parse::<u64>()
        .map_err(|error| {
            ContainerEngineError::permanent_from(
                "containerd returned an invalid semantic version",
                error,
            )
        })?;
    if major != CONTAINERD_MAJOR_VERSION {
        return Err(ContainerEngineError::permanent(format!(
            "unsupported containerd major version {major}; expected {CONTAINERD_MAJOR_VERSION}"
        )));
    }
    Ok(())
}

fn validate_plugin_ready(
    subject: &str,
    init_error: Option<&containerd_client::google::rpc::Status>,
) -> Result<(), ContainerEngineError> {
    let Some(init_error) = init_error else {
        return Ok(());
    };
    Err(ContainerEngineError::permanent(format!(
        "{subject} failed initialization: {}",
        init_error.message
    )))
}

fn platform_matches(
    candidate: &containerd_client::types::Platform,
    requested: &ContainerPlatform,
) -> bool {
    let (candidate_architecture, candidate_variant) =
        normalize_architecture(&candidate.architecture, &candidate.variant);
    let (requested_architecture, requested_variant) =
        normalize_architecture(requested.architecture(), requested.variant());

    normalize_os(&candidate.os) == normalize_os(requested.os())
        && candidate_architecture == requested_architecture
        && candidate_variant == requested_variant
}

fn platform_variant_suffix(variant: &str) -> String {
    if variant.is_empty() {
        String::new()
    } else {
        format!("/{variant}")
    }
}

fn io_error(reason: &'static str, error: io::Error) -> ContainerEngineError {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::Unsupported
    ) {
        ContainerEngineError::permanent_from(reason, error)
    } else {
        ContainerEngineError::retryable_from(reason, error)
    }
}

#[derive(Debug, Default)]
struct CleanupFailures(Vec<ContainerEngineError>);

impl CleanupFailures {
    fn push(&mut self, error: ContainerEngineError) {
        self.0.push(error);
    }

    fn into_result(self) -> Result<(), ContainerEngineError> {
        if self.0.is_empty() {
            return Ok(());
        }
        let permanent = self
            .0
            .iter()
            .any(|error| error.class() == ContainerErrorClass::Permanent);
        let source = CleanupFailureSet(self.0);
        if permanent {
            Err(ContainerEngineError::permanent_from(
                "containerd attempt cleanup failed",
                source,
            ))
        } else {
            Err(ContainerEngineError::retryable_from(
                "containerd attempt cleanup failed",
                source,
            ))
        }
    }
}

#[derive(Debug)]
struct CleanupFailureSet(Vec<ContainerEngineError>);

impl fmt::Display for CleanupFailureSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            fmt::Display::fmt(error, formatter)?;
        }
        Ok(())
    }
}

impl Error for CleanupFailureSet {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.first().map(|error| error as &(dyn Error + 'static))
    }
}

#[derive(Debug)]
struct CleanupRetryExhausted {
    timeout: Duration,
    last_error: Option<ContainerEngineError>,
}

impl fmt::Display for CleanupRetryExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cleanup did not finish within {:?}",
            self.timeout
        )?;
        if let Some(error) = &self.last_error {
            write!(formatter, "; last error: {error}")?;
        }
        Ok(())
    }
}

impl Error for CleanupRetryExhausted {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.last_error
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

#[derive(Debug)]
struct CreationRollbackFailure {
    creation: ContainerEngineError,
    rollback: ContainerEngineError,
}

impl fmt::Display for CreationRollbackFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "creation failed: {}; rollback failed: {}",
            self.creation, self.rollback
        )
    }
}

impl Error for CreationRollbackFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.creation)
    }
}

#[cfg(test)]
mod tests {
    use containerd_client::tonic::Code;

    use super::*;
    use crate::container::ContainerErrorClass;

    #[derive(Default)]
    struct RetryCleanupState {
        calls: Vec<tokio::time::Instant>,
        retryable_failures: usize,
    }

    fn retry_then_succeed(state: &mut RetryCleanupState) -> CleanupOperation<'_> {
        Box::pin(async move {
            state.calls.push(tokio::time::Instant::now());
            if state.retryable_failures == 0 {
                Ok(())
            } else {
                state.retryable_failures -= 1;
                Err(ContainerEngineError::retryable("temporary cleanup failure"))
            }
        })
    }

    fn fail_permanently(state: &mut RetryCleanupState) -> CleanupOperation<'_> {
        Box::pin(async move {
            state.calls.push(tokio::time::Instant::now());
            Err(ContainerEngineError::permanent("permanent cleanup failure"))
        })
    }

    #[derive(Default)]
    struct BudgetCleanupState {
        calls: Vec<tokio::time::Instant>,
    }

    fn slow_then_pending(state: &mut BudgetCleanupState) -> CleanupOperation<'_> {
        Box::pin(async move {
            state.calls.push(tokio::time::Instant::now());
            if state.calls.len() == 1 {
                tokio::time::sleep(Duration::from_secs(20)).await;
                Err(ContainerEngineError::retryable("temporary cleanup failure"))
            } else {
                std::future::pending().await
            }
        })
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_cleanup_uses_bounded_exponential_backoff() {
        let mut state = RetryCleanupState {
            retryable_failures: 2,
            ..Default::default()
        };

        retry_cleanup(&mut state, Duration::from_secs(30), retry_then_succeed)
            .await
            .unwrap();

        assert_eq!(state.calls.len(), 3);
        assert_eq!(state.calls[1] - state.calls[0], CLEANUP_BACKOFF_INITIAL);
        assert_eq!(state.calls[2] - state.calls[1], CLEANUP_BACKOFF_INITIAL * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_cleanup_failure_is_not_retried() {
        let mut state = RetryCleanupState::default();

        let error = retry_cleanup(&mut state, Duration::from_secs(30), fail_permanently)
            .await
            .unwrap_err();

        assert_eq!(error.class(), ContainerErrorClass::Permanent);
        assert_eq!(state.calls.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_retries_share_one_total_budget() {
        let mut state = BudgetCleanupState::default();
        let started = tokio::time::Instant::now();

        let error = retry_cleanup(&mut state, Duration::from_secs(30), slow_then_pending)
            .await
            .unwrap_err();

        assert_eq!(error.class(), ContainerErrorClass::Retryable);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(30)
        );
        assert_eq!(state.calls.len(), 2);
    }

    #[test]
    fn only_containerd_major_two_is_accepted() {
        for version in ["2", "2.0.0", "v2.1.4", "  v2.0.0-beta.1  "] {
            assert!(validate_version(version).is_ok(), "{version}");
        }
        for version in ["", "v", "1.7.27", "v3.0.0", "main"] {
            assert!(validate_version(version).is_err(), "{version}");
        }
    }

    #[test]
    fn runtime_info_accepts_containerd_and_canonical_any_type_urls() {
        let runtime = containerd_client::types::RuntimeInfo {
            name: "io.containerd.runc.v2".into(),
            ..Default::default()
        };
        let value = runtime.encode_to_vec();

        for type_url in [
            RUNTIME_INFO_TYPE,
            "/containerd.types.RuntimeInfo",
            "type.googleapis.com/containerd.types.RuntimeInfo",
        ] {
            let decoded = decode_runtime_info(Any {
                type_url: type_url.into(),
                value: value.clone(),
            })
            .unwrap();
            assert_eq!(decoded.name, runtime.name);
        }

        assert!(
            decode_runtime_info(Any {
                type_url: "containerd.types.RuntimeRequest".into(),
                value,
            })
            .is_err()
        );
    }

    #[test]
    fn status_mapping_distinguishes_contract_and_transport_failures() {
        for code in [
            Code::InvalidArgument,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::FailedPrecondition,
            Code::OutOfRange,
            Code::Unimplemented,
        ] {
            let error = image::rpc_error("operation failed", Status::new(code, "test"));
            assert_eq!(error.class(), ContainerErrorClass::Permanent, "{code:?}");
        }
        for code in [
            Code::Cancelled,
            Code::Unknown,
            Code::DeadlineExceeded,
            Code::ResourceExhausted,
            Code::Aborted,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
        ] {
            let error = image::rpc_error("operation failed", Status::new(code, "test"));
            assert_eq!(error.class(), ContainerErrorClass::Retryable, "{code:?}");
        }
    }

    #[test]
    fn resource_ids_are_attempt_scoped_and_metadata_safe() {
        let ids = ResourceIdGenerator::from_session([0xab; SESSION_BYTES]);

        let first = ids.next().unwrap();
        let second = ids.next().unwrap();

        assert_eq!(
            first,
            "solti-abababababababababababababababab-0000000000000001"
        );
        assert_eq!(
            second,
            "solti-abababababababababababababababab-0000000000000002"
        );
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
    }

    #[test]
    fn ownership_labels_identify_only_the_attempt() {
        let request = ContainerRequest {
            attempt_id: "run-secret-a3".to_owned(),
            task_name: solti_model::TaskId::new("task-a").unwrap(),
            generation: 7,
            attempt: 3,
            image: "registry.invalid/image:tag".to_owned(),
            command: Some(vec!["secret-command".to_owned()]),
            args: vec!["secret-argument".to_owned()],
            env: std::collections::BTreeMap::from([("SECRET".to_owned(), "value".to_owned())]),
            process_policy: crate::container::ContainerProcessPolicy::new(),
        };

        let labels = attempt_labels(&request, "resource-1", "session-1");

        assert_eq!(labels.len(), 6);
        assert_eq!(labels[LABEL_MANAGED_BY], MANAGED_BY);
        assert_eq!(labels[LABEL_SESSION], "session-1");
        assert_eq!(labels[LABEL_RESOURCE_ID], "resource-1");
        assert_eq!(labels[LABEL_TASK], "task-a");
        assert_eq!(labels[LABEL_GENERATION], "7");
        assert_eq!(labels[LABEL_ATTEMPT], "3");
        assert!(!labels.values().any(|value| value.contains("secret")));

        let mut changed = labels.clone();
        changed.insert(LABEL_SESSION.to_owned(), "another-session".to_owned());
        assert!(has_ownership_labels(&labels, &labels));
        assert!(!has_ownership_labels(&changed, &labels));
    }

    #[test]
    fn snapshot_identity_requires_our_id_parent_and_labels() {
        let expected_labels = HashMap::from([
            (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
            (LABEL_SESSION.to_owned(), "session-1".to_owned()),
        ]);
        let mut actual_labels = expected_labels.clone();
        actual_labels.insert("containerd.io/unrelated".to_owned(), "value".to_owned());

        assert!(snapshot_identity_matches(
            "resource-1",
            "parent-1",
            &actual_labels,
            "resource-1",
            Some("parent-1"),
            &expected_labels,
        ));
        assert!(snapshot_identity_matches(
            "resource-1",
            "another-parent",
            &actual_labels,
            "resource-1",
            None,
            &expected_labels,
        ));
        assert!(!snapshot_identity_matches(
            "foreign-resource",
            "parent-1",
            &actual_labels,
            "resource-1",
            Some("parent-1"),
            &expected_labels,
        ));
        assert!(!snapshot_identity_matches(
            "resource-1",
            "foreign-parent",
            &actual_labels,
            "resource-1",
            Some("parent-1"),
            &expected_labels,
        ));

        actual_labels.insert(LABEL_SESSION.to_owned(), "foreign-session".to_owned());
        assert!(!snapshot_identity_matches(
            "resource-1",
            "parent-1",
            &actual_labels,
            "resource-1",
            Some("parent-1"),
            &expected_labels,
        ));
    }

    #[test]
    fn container_identity_requires_our_snapshot_binding_and_labels() {
        let expected_labels = HashMap::from([
            (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
            (LABEL_SESSION.to_owned(), "session-1".to_owned()),
        ]);
        let mut actual_labels = expected_labels.clone();
        actual_labels.insert("containerd.io/unrelated".to_owned(), "value".to_owned());

        assert!(container_identity_matches(
            "resource-1",
            "overlayfs",
            "resource-1",
            &actual_labels,
            "resource-1",
            "overlayfs",
            &expected_labels,
        ));
        for (id, snapshotter, snapshot_key) in [
            ("foreign-resource", "overlayfs", "resource-1"),
            ("resource-1", "foreign-snapshotter", "resource-1"),
            ("resource-1", "overlayfs", "foreign-snapshot"),
        ] {
            assert!(!container_identity_matches(
                id,
                snapshotter,
                snapshot_key,
                &actual_labels,
                "resource-1",
                "overlayfs",
                &expected_labels,
            ));
        }

        actual_labels.insert(LABEL_SESSION.to_owned(), "foreign-session".to_owned());
        assert!(!container_identity_matches(
            "resource-1",
            "overlayfs",
            "resource-1",
            &actual_labels,
            "resource-1",
            "overlayfs",
            &expected_labels,
        ));
    }

    #[test]
    fn task_identity_requires_our_container() {
        for ownership in [Ownership::Absent, Ownership::Foreign, Ownership::Uncertain] {
            assert!(!task_identity_matches(
                ownership,
                "resource-1",
                "resource-1"
            ));
        }
        assert!(task_identity_matches(
            Ownership::Owned,
            "resource-1",
            "resource-1"
        ));
        assert!(!task_identity_matches(
            Ownership::Owned,
            "foreign-resource",
            "resource-1"
        ));
    }

    #[test]
    fn ownership_transitions_preserve_uncertain_failures() {
        assert_eq!(
            ownership_after_read_back(Ownership::Uncertain, OwnershipReadBack::Missing),
            Ownership::Absent,
        );
        assert_eq!(
            ownership_after_read_back(Ownership::Uncertain, OwnershipReadBack::Matching),
            Ownership::Owned,
        );
        assert_eq!(
            ownership_after_read_back(Ownership::Uncertain, OwnershipReadBack::Mismatched),
            Ownership::Foreign,
        );
        assert_eq!(
            ownership_after_read_back(Ownership::Uncertain, OwnershipReadBack::Unavailable),
            Ownership::Uncertain,
        );
    }

    #[test]
    fn cleanup_eligibility_is_dependency_safe_for_every_ownership_state() {
        let ownerships = [
            Ownership::Absent,
            Ownership::Foreign,
            Ownership::Owned,
            Ownership::Uncertain,
        ];

        for task in ownerships {
            for container in ownerships {
                for snapshot in ownerships {
                    let task_absent = task == Ownership::Absent;
                    let container_absent = container == Ownership::Absent;
                    let expected = CleanupEligibility {
                        confirm_task: task == Ownership::Uncertain,
                        delete_task: task == Ownership::Owned,
                        confirm_container: task_absent && container == Ownership::Uncertain,
                        delete_container: task_absent && container == Ownership::Owned,
                        confirm_snapshot: task_absent
                            && container_absent
                            && snapshot == Ownership::Uncertain,
                        delete_snapshot: task_absent
                            && container_absent
                            && snapshot == Ownership::Owned,
                        cleanup_io: true,
                    };

                    assert_eq!(
                        cleanup_eligibility(task, container, snapshot, true),
                        expected,
                        "task={task:?}, container={container:?}, snapshot={snapshot:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn cleanup_dependencies_open_only_after_confirmed_removal() {
        let snapshot = Ownership::Owned;
        let mut task = Ownership::Owned;
        let mut container = Ownership::Owned;

        task = ownership_after_delete(task, OwnershipDelete::Failed);
        assert!(!cleanup_eligibility(task, container, snapshot, true).delete_container);

        task = ownership_after_delete(task, OwnershipDelete::Missing);
        assert!(cleanup_eligibility(task, container, snapshot, true).delete_container);

        container = ownership_after_delete(container, OwnershipDelete::Failed);
        assert!(!cleanup_eligibility(task, container, snapshot, true).delete_snapshot);

        container = ownership_after_delete(container, OwnershipDelete::Removed);
        assert!(cleanup_eligibility(task, container, snapshot, true).delete_snapshot);
        assert!(cleanup_eligibility(task, container, snapshot, true).cleanup_io);
        assert!(!cleanup_eligibility(task, container, snapshot, false).cleanup_io);
    }

    #[test]
    fn only_retryable_statuses_have_ambiguous_create_outcomes() {
        for code in [
            Code::Cancelled,
            Code::Unknown,
            Code::DeadlineExceeded,
            Code::ResourceExhausted,
            Code::Aborted,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
        ] {
            assert!(ambiguous_create_status(&Status::new(code, "test")));
        }
        for code in [
            Code::InvalidArgument,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::FailedPrecondition,
            Code::OutOfRange,
            Code::Unimplemented,
        ] {
            assert!(!ambiguous_create_status(&Status::new(code, "test")));
        }
    }

    #[test]
    fn plugin_platforms_use_oci_normalization() {
        let amd64_v1 = containerd_client::types::Platform {
            os: "linux".to_owned(),
            architecture: "x86_64".to_owned(),
            variant: "v1".to_owned(),
            os_version: String::new(),
        };
        let arm64 = ContainerPlatform::new("linux", "arm64", "");
        let amd64 = ContainerPlatform::new("linux", "amd64", "");

        assert!(platform_matches(&amd64_v1, &amd64));
        assert!(!platform_matches(&amd64_v1, &arm64));
    }
}
