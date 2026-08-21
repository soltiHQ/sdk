//! Native containerd 2.x lifecycle adapter.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use containerd_client::{
    Client,
    services::v1::{
        Container, CreateContainerRequest, CreateTaskRequest, DeleteContainerRequest,
        DeleteTaskRequest, GetContainerRequest, GetRequest, KillRequest, PluginInfoRequest,
        PluginsRequest, StartRequest, WaitRequest,
        container::Runtime,
        snapshots::{
            MountsRequest, PrepareSnapshotRequest, RemoveSnapshotRequest, StatSnapshotRequest,
        },
    },
    tonic::{
        Code, Status,
        metadata::{Ascii, MetadataValue},
    },
    types::v1::Status as ProcessStatus,
};
use prost::Message;
use prost_types::Any;
use tracing::warn;

use super::{
    ContainerPlatform, ContainerdConfig,
    cleanup::{CleanupDomain, CleanupReservation},
    config::{normalize_architecture, normalize_os},
    image::{self, ImageResolveRequest},
    io_domain::{IoDomain, IoPreparation, ManagedAttemptIo},
    rpc::{
        AttemptMutation, AttemptRpc, AttemptWait, ClientAttemptRpc, MutationRequest, MutationResult,
    },
};
use crate::container::{
    ContainerAttempt, ContainerEngine, ContainerEngineError, ContainerEngineInfo,
    ContainerErrorClass, ContainerExitStatus, ContainerOutput, ContainerRequest,
};

#[cfg(test)]
use super::io::AttemptIo;

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

const LABEL_ATTEMPT: &str = "solti.io/attempt";
const LABEL_GENERATION: &str = "solti.io/generation";
const LABEL_MANAGED_BY: &str = "solti.io/managed-by";
const LABEL_RESOURCE_ID: &str = "solti.io/resource-id";
const LABEL_SESSION: &str = "solti.io/session";
const LABEL_TASK: &str = "solti.io/task";
const MANAGED_BY: &str = "solti-exec";

/// Ownership decision for one attempt resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ownership {
    /// The resource is confirmed missing.
    Absent,
    /// The resource exists but does not belong to this attempt.
    Foreign,
    /// The resource is confirmed to belong to this attempt.
    Owned,
    /// A create request may still commit remotely.
    CreateUncertain,
    /// A delete result requires identity read-back.
    DeleteUncertain,
}

/// Mutating RPC stage whose remote result is not yet settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationStage {
    /// Snapshot creation may still commit.
    PrepareSnapshot,
    /// Container creation may still commit.
    CreateContainer,
    /// Runtime task creation may still commit.
    CreateTask,
    /// Runtime task start may still commit.
    StartTask,
    /// Runtime task termination may still commit.
    KillTask,
    /// Runtime task deletion may still commit.
    DeleteTask,
    /// Container deletion may still commit.
    DeleteContainer,
    /// Snapshot deletion may still commit.
    DeleteSnapshot,
}

impl MutationStage {
    /// Returns the stable stage name used by diagnostics.
    const fn as_str(self) -> &'static str {
        match self {
            Self::PrepareSnapshot => "prepare_snapshot",
            Self::CreateContainer => "create_container",
            Self::CreateTask => "create_task",
            Self::StartTask => "start_task",
            Self::KillTask => "kill_task",
            Self::DeleteTask => "delete_task",
            Self::DeleteContainer => "delete_container",
            Self::DeleteSnapshot => "delete_snapshot",
        }
    }
}

/// Remote mutation that must settle before ownership read-back or retry.
struct InFlightMutation {
    /// Operation whose client result is still owned by the engine runtime.
    stage: MutationStage,
    /// Cancellation-safe RPC owner or a terminal worker failure.
    owner: MutationOwner,
}

/// State of one engine-owned mutation task.
enum MutationOwner {
    /// The client request is still running or has a result to collect.
    Running(AttemptMutation),
    /// The request task stopped without a result.
    Lost,
}

/// Native adapter for a configured containerd 2.x endpoint.
///
/// Construction validates the endpoint major version, snapshotter, platform, and OCI runtime.
/// The adapter never scans for sockets or starts a daemon.
pub struct ContainerdEngine {
    /// Client used for image resolution and compatibility probes.
    client: Arc<Client>,
    /// Attempt-scoped RPC adapter shared by active and deferred cleanup.
    attempt_rpc: Arc<dyn AttemptRpc>,
    /// Bounded owner for attempt resources after lifecycle cancellation.
    cleanup: CleanupDomain,
    /// Bounded blocking owner for local attempt I/O.
    io_domain: IoDomain,
    /// Validated engine settings.
    config: ContainerdConfig,
    /// Namespace metadata attached to image and probe operations.
    namespace: MetadataValue<Ascii>,
    /// Process-local attempt identifier source.
    ids: ResourceIdGenerator,
}

impl ContainerdEngine {
    /// Connects to an explicit containerd 2.x Unix socket.
    ///
    /// The method starts engine-local cleanup and blocking I/O threads. The
    /// containerd channel is created and driven inside the cleanup thread.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid settings, cleanup runtime startup failure,
    /// connection failure, another major version, or an incompatible plugin.
    pub async fn connect(config: ContainerdConfig) -> Result<Self, ContainerEngineError> {
        let namespace = config.validate()?;
        let io_domain = IoDomain::start(config.cleanup_capacity())?;
        let (client, mutation_executor, cleanup) = CleanupDomain::start(
            config.socket().to_owned(),
            config.control_timeout(),
            config.cleanup_capacity(),
        )
        .await?;
        let attempt_rpc: Arc<dyn AttemptRpc> = Arc::new(ClientAttemptRpc::new(
            Arc::clone(&client),
            namespace.clone(),
            mutation_executor,
        ));
        let ids = ResourceIdGenerator::random()?;
        let engine = Self {
            client,
            attempt_rpc,
            cleanup,
            io_domain,
            config,
            namespace,
            ids,
        };
        engine.probe().await?;
        Ok(engine)
    }

    /// Checks major version 2 and the configured snapshotter, platform, and OCI runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when containerd is unavailable or incompatible with
    /// the configured version, snapshotter, platform, or runtime.
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

    /// Stops lifecycle admission and waits for accepted create lifecycles and
    /// attempt ownership.
    ///
    /// Call this after supervisors that use the engine have stopped. The
    /// method is terminal and idempotent. Later create calls fail before image
    /// resolution because lifecycle admission remains closed. Local I/O shutdown
    /// still runs when remote cleanup reports an error. A configured duration
    /// beyond Tokio's representable `Instant` range waits without a local
    /// deadline and remains cancellation-safe.
    ///
    /// # Errors
    ///
    /// Returns a retryable error when accepted ownership does not finish
    /// within the configured cleanup timeout. Returns a permanent error when
    /// either worker loses or quarantines ownership.
    pub async fn shutdown(&self) -> Result<(), ContainerEngineError> {
        let deadline = tokio::time::Instant::now().checked_add(self.config.cleanup_timeout());
        let cleanup = self.cleanup.shutdown_until(deadline).await;
        let io = self.io_domain.shutdown_until(deadline).await;
        combine_shutdown_results(cleanup, io)
    }

    /// Verifies snapshotter readiness and platform support.
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

    /// Verifies OCI runtime readiness and platform support.
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

    /// Creates one admitted native lifecycle and resolves its shared image.
    ///
    /// Lifecycle admission precedes image resolution and remains charged through
    /// the attempt or deferred cleanup. Image transfer itself is not retained in
    /// [`AttemptState`] for deferred cleanup.
    async fn create_owned_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<ContainerdAttempt, ContainerEngineError> {
        let resource_id = self.ids.next()?;
        let labels = attempt_labels(&request, &resource_id, self.ids.session());
        let cleanup = self.cleanup.try_reserve()?;
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

        let io = self
            .io_domain
            .try_prepare(self.config.io_root().to_owned(), resource_id.clone())?;
        let state = AttemptState::new(
            Arc::clone(&self.attempt_rpc),
            self.config.snapshotter().to_owned(),
            resource_id,
            labels,
            AttemptIoState::Preparing(io),
            AttemptTimeouts {
                control: self.config.control_timeout(),
                cleanup: self.config.cleanup_timeout(),
            },
        );
        let mut attempt = ContainerdAttempt::new(state, cleanup);

        let create_result = async {
            attempt.state_mut().settle_io_preparation().await?;
            attempt
                .state_mut()
                .create_resources(
                    &resolved.reference,
                    &resolved.chain_id,
                    self.config.runtime(),
                    spec,
                )
                .await
        }
        .await;
        match create_result {
            Ok(()) => Ok(attempt),
            Err(creation) => {
                let rollback = attempt.state_mut().cleanup_owned_with_retry().await;
                match rollback {
                    Ok(()) => {
                        attempt.disarm_if_released();
                        Err(creation)
                    }
                    Err(rollback) => {
                        attempt.handoff();
                        Err(ContainerEngineError::permanent_from(
                            "containerd attempt creation failed and rollback was incomplete",
                            CreationRollbackFailure { creation, rollback },
                        ))
                    }
                }
            }
        }
    }
}

/// Decodes and validates containerd runtime information.
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

#[async_trait]
impl crate::container::ContainerEngineFinalizer for ContainerdEngine {
    async fn shutdown(&self) -> Result<(), ContainerEngineError> {
        ContainerdEngine::shutdown(self).await
    }
}

/// Local attempt I/O retained across preparation and removal cancellation.
enum AttemptIoState {
    /// Blocking preparation is running or has a result to collect.
    Preparing(IoPreparation),
    /// Prepared local resources belong to the active attempt.
    Ready(ManagedAttemptIo),
    /// No local resources remain owned.
    Absent,
    /// The I/O worker lost safe ownership progress.
    Lost,
}

impl AttemptIoState {
    /// Returns whether local ownership may still exist.
    fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    /// Returns whether all local ownership is released.
    fn is_released(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Borrows prepared local I/O.
    fn ready(&self) -> Option<&ManagedAttemptIo> {
        match self {
            Self::Ready(io) => Some(io),
            Self::Preparing(_) | Self::Absent | Self::Lost => None,
        }
    }

    /// Borrows mutable prepared local I/O.
    fn ready_mut(&mut self) -> Option<&mut ManagedAttemptIo> {
        match self {
            Self::Ready(io) => Some(io),
            Self::Preparing(_) | Self::Absent | Self::Lost => None,
        }
    }

    /// Returns the stable state label used by cleanup diagnostics.
    fn status_label(&self) -> &'static str {
        match self {
            Self::Preparing(_) => "preparing",
            Self::Ready(io) if io.is_lost() => "lost",
            Self::Ready(_) => "ready",
            Self::Absent => "absent",
            Self::Lost => "lost",
        }
    }
}

/// Mutable ownership and lifecycle state for one attempt.
pub(super) struct AttemptState {
    /// RPC adapter used by the active lifecycle and deferred cleanup.
    rpc: Arc<dyn AttemptRpc>,
    /// Snapshotter that owns the active snapshot.
    snapshotter: String,
    /// Attempt-scoped identifier shared by all native resources.
    resource_id: String,
    /// Labels required before uncertain resources may be adopted.
    labels: HashMap<String, String>,
    /// Local output resources retained until remote task cleanup completes.
    io: AttemptIoState,
    /// Snapshot ownership known by this process.
    snapshot: Ownership,
    /// Parent chain identifier required to verify the attempt snapshot.
    snapshot_parent: Option<String>,
    /// Container metadata ownership known by this process.
    container: Ownership,
    /// Runtime task ownership known by this process.
    task: Ownership,
    /// Process identifier returned by task creation or ownership read-back.
    task_pid: Option<u32>,
    /// Output path passed to the owned runtime task.
    task_stdout: Option<String>,
    /// Error path passed to the owned runtime task.
    task_stderr: Option<String>,
    /// Armed task wait owned by this attempt.
    wait: Option<AttemptWait>,
    /// Remote mutation that may still commit after local cancellation.
    in_flight: Option<InFlightMutation>,
    /// Exit status observed for the runtime task.
    exit_status: Option<ContainerExitStatus>,
    /// Whether task start was requested.
    start_requested: bool,
    /// Whether task start returned success.
    start_confirmed: bool,
    /// Whether a start request may still commit remotely.
    start_uncertain: bool,
    /// Whether task termination returned an accepted result.
    termination_sent: bool,
    /// Whether termination must be followed by task wait.
    termination_requires_wait: bool,
    /// Deadline for one containerd control RPC.
    control_timeout: Duration,
    /// Duration of one cleanup retry window.
    cleanup_timeout: Duration,
}

/// Deadlines used by one attempt lifecycle.
#[derive(Clone, Copy)]
struct AttemptTimeouts {
    /// Deadline for one containerd control RPC.
    control: Duration,
    /// Duration of one cleanup retry window.
    cleanup: Duration,
}

impl AttemptState {
    /// Creates dormant ownership state for one admitted attempt.
    fn new(
        rpc: Arc<dyn AttemptRpc>,
        snapshotter: String,
        resource_id: String,
        labels: HashMap<String, String>,
        io: AttemptIoState,
        timeouts: AttemptTimeouts,
    ) -> Self {
        Self {
            rpc,
            snapshotter,
            resource_id,
            labels,
            io,
            snapshot: Ownership::Absent,
            snapshot_parent: None,
            container: Ownership::Absent,
            task: Ownership::Absent,
            task_pid: None,
            task_stdout: None,
            task_stderr: None,
            wait: None,
            in_flight: None,
            exit_status: None,
            start_requested: false,
            start_confirmed: false,
            start_uncertain: false,
            termination_sent: false,
            termination_requires_wait: false,
            control_timeout: timeouts.control,
            cleanup_timeout: timeouts.cleanup,
        }
    }

    /// Waits for accepted I/O preparation without removing its owner.
    ///
    /// Cancellation leaves the preparation receiver inside this state.
    ///
    /// # Errors
    ///
    /// Returns the preparation failure. A lost worker result keeps this
    /// attempt in the terminal `Lost` state.
    async fn settle_io_preparation(&mut self) -> Result<(), ContainerEngineError> {
        let result = match &mut self.io {
            AttemptIoState::Preparing(preparation) => preparation.join().await,
            AttemptIoState::Ready(_) | AttemptIoState::Absent => return Ok(()),
            AttemptIoState::Lost => {
                return Err(ContainerEngineError::permanent(
                    "containerd I/O ownership is lost",
                ));
            }
        };

        match result {
            Ok(io) => {
                self.io = AttemptIoState::Ready(io);
                Ok(())
            }
            Err(error) => {
                let lost = matches!(
                    &self.io,
                    AttemptIoState::Preparing(preparation) if preparation.is_lost()
                );
                self.io = if lost {
                    AttemptIoState::Lost
                } else {
                    AttemptIoState::Absent
                };
                Err(error)
            }
        }
    }

    /// Creates the snapshot, container, runtime task, output, and wait owner.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource cannot be created, verified, or armed.
    async fn create_resources(
        &mut self,
        image_reference: &str,
        parent_snapshot: &str,
        runtime: &str,
        spec: Vec<u8>,
    ) -> Result<(), ContainerEngineError> {
        self.snapshot_parent = Some(parent_snapshot.to_owned());
        self.snapshot = Ownership::CreateUncertain;
        self.begin_mutation(
            MutationStage::PrepareSnapshot,
            MutationRequest::PrepareSnapshot(PrepareSnapshotRequest {
                snapshotter: self.snapshotter.clone(),
                key: self.resource_id.clone(),
                parent: parent_snapshot.to_owned(),
                labels: self.labels.clone(),
            }),
        );
        let MutationResult::PrepareSnapshot(snapshot) =
            self.await_mutation(MutationStage::PrepareSnapshot).await?
        else {
            unreachable!("snapshot preparation returned another mutation result")
        };
        let mounts = match snapshot {
            Ok(response) => {
                self.snapshot = Ownership::Owned;
                response.mounts
            }
            Err(status) if status.code() == Code::AlreadyExists => {
                self.snapshot = Ownership::Foreign;
                return Err(image::rpc_error(
                    "containerd snapshot prepare failed",
                    status,
                ));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.snapshot = Ownership::CreateUncertain;
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
                            event = "containerd.create_outcome_unverified",
                            resource_id = %self.resource_id,
                            stage = "snapshot",
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
                self.snapshot = Ownership::Absent;
                return Err(image::rpc_error(
                    "containerd snapshot prepare failed",
                    status,
                ));
            }
        };

        self.container = Ownership::CreateUncertain;
        self.begin_mutation(
            MutationStage::CreateContainer,
            MutationRequest::CreateContainer(CreateContainerRequest {
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
            }),
        );
        let MutationResult::CreateContainer(container) =
            self.await_mutation(MutationStage::CreateContainer).await?
        else {
            unreachable!("container creation returned another mutation result")
        };
        match *container {
            Ok(_) => {
                self.container = Ownership::Owned;
            }
            Err(status) if status.code() == Code::AlreadyExists => {
                self.container = Ownership::Foreign;
                return Err(image::rpc_error(
                    "containerd container create failed",
                    status,
                ));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.container = Ownership::CreateUncertain;
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
                            event = "containerd.create_outcome_unverified",
                            resource_id = %self.resource_id,
                            stage = "container",
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
                self.container = Ownership::Absent;
                return Err(image::rpc_error(
                    "containerd container create failed",
                    status,
                ));
            }
        }

        let io = self.io.ready().ok_or_else(|| {
            ContainerEngineError::permanent("containerd attempt has no output pipes")
        })?;
        let stdout = io
            .stdout_path()
            .to_str()
            .ok_or_else(|| {
                ContainerEngineError::permanent("containerd stdout path is not valid UTF-8")
            })?
            .to_owned();
        let stderr = io
            .stderr_path()
            .to_str()
            .ok_or_else(|| {
                ContainerEngineError::permanent("containerd stderr path is not valid UTF-8")
            })?
            .to_owned();
        self.task_stdout = Some(stdout.clone());
        self.task_stderr = Some(stderr.clone());
        self.task = Ownership::CreateUncertain;
        self.begin_mutation(
            MutationStage::CreateTask,
            MutationRequest::CreateTask(CreateTaskRequest {
                container_id: self.resource_id.clone(),
                rootfs: mounts,
                stdin: String::new(),
                stdout,
                stderr,
                terminal: false,
                checkpoint: None,
                options: None,
                runtime_path: String::new(),
            }),
        );
        let MutationResult::CreateTask(task) =
            self.await_mutation(MutationStage::CreateTask).await?
        else {
            unreachable!("task creation returned another mutation result")
        };
        match task {
            Ok(response) => {
                self.task = Ownership::Owned;
                self.task_pid = Some(response.pid);
            }
            Err(status) if status.code() == Code::AlreadyExists => {
                self.task = Ownership::Foreign;
                return Err(image::rpc_error("containerd task create failed", status));
            }
            Err(status) if ambiguous_create_status(&status) => {
                self.task = Ownership::CreateUncertain;
                match self.confirm_task_ownership().await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(image::rpc_error("containerd task create failed", status));
                    }
                    Err(readback) => {
                        warn!(
                            event = "containerd.create_outcome_unverified",
                            resource_id = %self.resource_id,
                            stage = "task",
                            error = %readback,
                            "containerd task create outcome could not be verified",
                        );
                        return Err(image::rpc_error("containerd task create failed", status));
                    }
                }
            }
            Err(status) => {
                self.task = Ownership::Absent;
                return Err(image::rpc_error("containerd task create failed", status));
            }
        }

        self.io
            .ready_mut()
            .expect("attempt I/O exists until cleanup")
            .activate()?;
        self.wait = Some(
            self.rpc
                .arm_wait(
                    WaitRequest {
                        container_id: self.resource_id.clone(),
                        exec_id: String::new(),
                    },
                    self.control_timeout,
                )
                .await?,
        );
        Ok(())
    }

    /// Reads snapshot metadata and verifies the expected parent and labels.
    async fn confirm_snapshot_ownership(
        &mut self,
        expected_parent: &str,
    ) -> Result<bool, ContainerEngineError> {
        self.confirm_snapshot_ownership_with_parent(expected_parent)
            .await
    }

    /// Reads snapshot metadata and verifies cleanup ownership labels.
    async fn confirm_snapshot_ownership_for_cleanup(
        &mut self,
    ) -> Result<bool, ContainerEngineError> {
        let expected_parent = self.snapshot_parent.clone().ok_or_else(|| {
            ContainerEngineError::permanent("containerd attempt has no expected snapshot parent")
        })?;
        self.confirm_snapshot_ownership_with_parent(&expected_parent)
            .await
    }

    /// Verifies snapshot ownership with the expected parent constraint.
    ///
    /// # Errors
    ///
    /// Returns an error when read-back fails or another resource owns the ID.
    async fn confirm_snapshot_ownership_with_parent(
        &mut self,
        expected_parent: &str,
    ) -> Result<bool, ContainerEngineError> {
        if matches!(self.snapshot, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        let previous = self.snapshot;
        let response = self
            .rpc
            .stat_snapshot(
                StatSnapshotRequest {
                    snapshotter: self.snapshotter.clone(),
                    key: self.resource_id.clone(),
                },
                self.control_timeout,
            )
            .await;
        let info = match response {
            Ok(response) => match response.info {
                Some(info) => info,
                None => {
                    self.snapshot =
                        ownership_after_read_back(self.snapshot, OwnershipReadBack::Unavailable);
                    return Err(ContainerEngineError::permanent(
                        "containerd snapshot read-back returned no metadata",
                    ));
                }
            },
            Err(status) if status.code() == Code::NotFound => {
                self.snapshot =
                    ownership_after_read_back(self.snapshot, OwnershipReadBack::Missing);
                if previous == Ownership::CreateUncertain {
                    return Err(ContainerEngineError::retryable(
                        "containerd snapshot create outcome remains unresolved",
                    ));
                }
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

    /// Returns mounts for the admitted snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when containerd cannot return snapshot mounts.
    async fn snapshot_mounts(
        &mut self,
    ) -> Result<Vec<containerd_client::types::Mount>, ContainerEngineError> {
        self.rpc
            .mount_snapshot(
                MountsRequest {
                    snapshotter: self.snapshotter.clone(),
                    key: self.resource_id.clone(),
                },
                self.control_timeout,
            )
            .await
            .map(|response| response.mounts)
            .map_err(|status| image::rpc_error("containerd snapshot mounts lookup failed", status))
    }

    /// Reads and verifies container identity metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when read-back fails or another resource owns the ID.
    async fn confirm_container_ownership(&mut self) -> Result<bool, ContainerEngineError> {
        if matches!(self.container, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        let previous = self.container;
        let response = self
            .rpc
            .get_container(
                GetContainerRequest {
                    id: self.resource_id.clone(),
                },
                self.control_timeout,
            )
            .await;
        let container = match response {
            Ok(response) => match response.container {
                Some(container) => container,
                None => {
                    self.container =
                        ownership_after_read_back(self.container, OwnershipReadBack::Unavailable);
                    return Err(ContainerEngineError::permanent(
                        "containerd container read-back returned no metadata",
                    ));
                }
            },
            Err(status) if status.code() == Code::NotFound => {
                self.container =
                    ownership_after_read_back(self.container, OwnershipReadBack::Missing);
                if previous == Ownership::CreateUncertain {
                    return Err(ContainerEngineError::retryable(
                        "containerd container create outcome remains unresolved",
                    ));
                }
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

    /// Reads and verifies runtime task identity.
    ///
    /// # Errors
    ///
    /// Returns an error when read-back fails or another process owns the ID.
    async fn confirm_task_ownership(&mut self) -> Result<bool, ContainerEngineError> {
        if matches!(self.task, Ownership::Absent | Ownership::Foreign) {
            return Ok(false);
        }
        if self.container != Ownership::Owned {
            return Err(ContainerEngineError::permanent(
                "cannot adopt a containerd task without its owned container metadata",
            ));
        }
        let previous = self.task;
        let response = self
            .rpc
            .get_task(
                GetRequest {
                    container_id: self.resource_id.clone(),
                    exec_id: String::new(),
                },
                self.control_timeout,
            )
            .await;
        let process = match response {
            Ok(response) => match response.process {
                Some(process) => process,
                None => {
                    self.task =
                        ownership_after_read_back(self.task, OwnershipReadBack::Unavailable);
                    return Err(ContainerEngineError::permanent(
                        "containerd task read-back returned no process",
                    ));
                }
            },
            Err(status) if status.code() == Code::NotFound => {
                self.task = ownership_after_read_back(self.task, OwnershipReadBack::Missing);
                if previous == Ownership::CreateUncertain {
                    return Err(ContainerEngineError::retryable(
                        "containerd task create outcome remains unresolved",
                    ));
                }
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
        let matches = task_identity_matches(
            self.container,
            TaskIdentity {
                container_id: &process.container_id,
                pid: process.pid,
                stdout: &process.stdout,
                stderr: &process.stderr,
            },
            ExpectedTaskIdentity {
                resource_id: &self.resource_id,
                pid: self.task_pid,
                stdout: self.task_stdout.as_deref(),
                stderr: self.task_stderr.as_deref(),
            },
        );
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
        self.task_pid = Some(process.pid);
        if self.start_uncertain {
            match ProcessStatus::try_from(process.status) {
                Ok(
                    ProcessStatus::Running
                    | ProcessStatus::Stopped
                    | ProcessStatus::Paused
                    | ProcessStatus::Pausing,
                ) => {
                    self.start_confirmed = true;
                    self.start_uncertain = false;
                }
                Ok(ProcessStatus::Created | ProcessStatus::Unknown) | Err(_) => {
                    self.task = Ownership::DeleteUncertain;
                    return Err(ContainerEngineError::retryable(
                        "containerd task start outcome remains unresolved",
                    ));
                }
            }
        }
        Ok(true)
    }

    /// Confirms that a completed task deletion did not expose a replacement.
    async fn confirm_task_absent_after_delete(&mut self) -> Result<(), ContainerEngineError> {
        match self.confirm_task_ownership().await? {
            false => Ok(()),
            true => Err(ContainerEngineError::retryable(
                "containerd task still exists after deletion",
            )),
        }
    }

    /// Confirms that a completed container deletion did not expose a replacement.
    async fn confirm_container_absent_after_delete(&mut self) -> Result<(), ContainerEngineError> {
        match self.confirm_container_ownership().await? {
            false => Ok(()),
            true => Err(ContainerEngineError::retryable(
                "containerd container still exists after deletion",
            )),
        }
    }

    /// Confirms that a completed snapshot deletion did not expose a replacement.
    async fn confirm_snapshot_absent_after_delete(&mut self) -> Result<(), ContainerEngineError> {
        match self.confirm_snapshot_ownership_for_cleanup().await? {
            false => Ok(()),
            true => Err(ContainerEngineError::retryable(
                "containerd snapshot still exists after deletion",
            )),
        }
    }

    /// Arms task wait when the owned task has no active wait owner.
    ///
    /// # Errors
    ///
    /// Returns an error when task wait cannot be armed.
    async fn ensure_wait_armed(&mut self) -> Result<(), ContainerEngineError> {
        if self.wait.is_none() && self.task == Ownership::Owned {
            self.wait = Some(
                self.rpc
                    .arm_wait(
                        WaitRequest {
                            container_id: self.resource_id.clone(),
                            exec_id: String::new(),
                        },
                        self.control_timeout,
                    )
                    .await?,
            );
        }
        Ok(())
    }

    /// Starts the owned runtime task once.
    ///
    /// # Errors
    ///
    /// Returns an error for unavailable ownership or a failed start request.
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
        self.begin_mutation(
            MutationStage::StartTask,
            MutationRequest::StartTask(StartRequest {
                container_id: self.resource_id.clone(),
                exec_id: String::new(),
            }),
        );
        let MutationResult::StartTask(result) =
            self.await_mutation(MutationStage::StartTask).await?
        else {
            unreachable!("task start returned another mutation result")
        };
        match result {
            Ok(_) => {
                self.start_confirmed = true;
                self.start_uncertain = false;
                Ok(())
            }
            Err(status) => {
                self.start_uncertain = ambiguous_create_status(&status);
                Err(image::rpc_error("containerd task start failed", status))
            }
        }
    }

    /// Waits for the runtime task and caches its exit status.
    ///
    /// # Errors
    ///
    /// Returns an error when wait cannot be armed or its worker fails.
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
            .join()
            .await
        {
            Ok(Ok(response)) => response,
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

    /// Sends one terminating signal to an active owned task.
    ///
    /// # Errors
    ///
    /// Returns an error when containerd rejects or cannot deliver the signal.
    async fn terminate_inner(&mut self) -> Result<(), ContainerEngineError> {
        if self.task != Ownership::Owned || self.exit_status.is_some() || !self.start_requested {
            return Ok(());
        }
        if self.termination_sent {
            return Ok(());
        }

        if let Err(error) = self.ensure_wait_armed().await {
            warn!(
                event = "containerd.wait_rearm_failed",
                resource_id = %self.resource_id,
                error = %error,
                "containerd wait could not be re-armed before termination",
            );
        }

        self.begin_mutation(
            MutationStage::KillTask,
            MutationRequest::KillTask(KillRequest {
                container_id: self.resource_id.clone(),
                exec_id: String::new(),
                signal: SIGKILL,
                all: true,
            }),
        );
        let MutationResult::KillTask(result) = self.await_mutation(MutationStage::KillTask).await?
        else {
            unreachable!("task termination returned another mutation result")
        };
        match result {
            Ok(()) => {
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

    /// Runs one dependency-ordered cleanup attempt.
    ///
    /// # Errors
    ///
    /// Returns all resource cleanup failures observed during this attempt.
    async fn cleanup_owned(&mut self) -> Result<(), ContainerEngineError> {
        let mut failures = CleanupFailures::default();

        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .confirm_task
        {
            match self.confirm_task_ownership().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }

        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
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
                && cleanup_eligibility(
                    self.task,
                    self.container,
                    self.snapshot,
                    self.io.is_present(),
                )
                .delete_task
            {
                self.task = Ownership::DeleteUncertain;
                self.begin_mutation(
                    MutationStage::DeleteTask,
                    MutationRequest::DeleteTask(DeleteTaskRequest {
                        container_id: self.resource_id.clone(),
                    }),
                );
                let MutationResult::DeleteTask(result) =
                    self.await_mutation(MutationStage::DeleteTask).await?
                else {
                    unreachable!("task deletion returned another mutation result")
                };
                match result {
                    Ok(_) => {
                        self.task = Ownership::DeleteUncertain;
                        if let Err(error) = self.confirm_task_absent_after_delete().await {
                            failures.push(error);
                        }
                    }
                    Err(status) if status.code() == Code::NotFound => {
                        self.task = Ownership::DeleteUncertain;
                        if let Err(error) = self.confirm_task_absent_after_delete().await {
                            failures.push(error);
                        }
                    }
                    Err(status) => {
                        self.task = if ambiguous_create_status(&status) {
                            Ownership::DeleteUncertain
                        } else {
                            Ownership::Owned
                        };
                        if self.task == Ownership::DeleteUncertain
                            && let Err(error) = self.confirm_task_absent_after_delete().await
                        {
                            failures.push(error);
                        }
                        if let Some(error) = wait_error {
                            failures.push(error);
                        }
                        if self.task == Ownership::Owned {
                            failures
                                .push(image::rpc_error("containerd task cleanup failed", status));
                        }
                    }
                }
            }
            if self.task == Ownership::Absent {
                self.abort_wait();
            }
        }

        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .confirm_container
        {
            match self.confirm_container_ownership().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }
        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .delete_container
        {
            self.container = Ownership::DeleteUncertain;
            self.begin_mutation(
                MutationStage::DeleteContainer,
                MutationRequest::DeleteContainer(DeleteContainerRequest {
                    id: self.resource_id.clone(),
                }),
            );
            let MutationResult::DeleteContainer(result) =
                self.await_mutation(MutationStage::DeleteContainer).await?
            else {
                unreachable!("container deletion returned another mutation result")
            };
            match result {
                Ok(_) => {
                    self.container = Ownership::DeleteUncertain;
                    if let Err(error) = self.confirm_container_absent_after_delete().await {
                        failures.push(error);
                    }
                }
                Err(status) if status.code() == Code::NotFound => {
                    self.container = Ownership::DeleteUncertain;
                    if let Err(error) = self.confirm_container_absent_after_delete().await {
                        failures.push(error);
                    }
                }
                Err(status) => {
                    self.container = if ambiguous_create_status(&status) {
                        Ownership::DeleteUncertain
                    } else {
                        Ownership::Owned
                    };
                    if self.container == Ownership::DeleteUncertain
                        && let Err(error) = self.confirm_container_absent_after_delete().await
                    {
                        failures.push(error);
                    }
                    if self.container == Ownership::Owned {
                        failures.push(image::rpc_error(
                            "containerd container cleanup failed",
                            status,
                        ));
                    }
                }
            }
        }

        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .confirm_snapshot
        {
            match self.confirm_snapshot_ownership_for_cleanup().await {
                Ok(_) => {}
                Err(error) => failures.push(error),
            }
        }
        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .delete_snapshot
        {
            self.snapshot = Ownership::DeleteUncertain;
            self.begin_mutation(
                MutationStage::DeleteSnapshot,
                MutationRequest::RemoveSnapshot(RemoveSnapshotRequest {
                    snapshotter: self.snapshotter.clone(),
                    key: self.resource_id.clone(),
                }),
            );
            let MutationResult::RemoveSnapshot(result) =
                self.await_mutation(MutationStage::DeleteSnapshot).await?
            else {
                unreachable!("snapshot deletion returned another mutation result")
            };
            match result {
                Ok(_) => {
                    self.snapshot = Ownership::DeleteUncertain;
                    if let Err(error) = self.confirm_snapshot_absent_after_delete().await {
                        failures.push(error);
                    }
                }
                Err(status) if status.code() == Code::NotFound => {
                    self.snapshot = Ownership::DeleteUncertain;
                    if let Err(error) = self.confirm_snapshot_absent_after_delete().await {
                        failures.push(error);
                    }
                }
                Err(status) => {
                    self.snapshot = if ambiguous_create_status(&status) {
                        Ownership::DeleteUncertain
                    } else {
                        Ownership::Owned
                    };
                    if self.snapshot == Ownership::DeleteUncertain
                        && let Err(error) = self.confirm_snapshot_absent_after_delete().await
                    {
                        failures.push(error);
                    }
                    if self.snapshot == Ownership::Owned {
                        failures.push(image::rpc_error(
                            "containerd snapshot cleanup failed",
                            status,
                        ));
                    }
                }
            }
        }

        if cleanup_eligibility(
            self.task,
            self.container,
            self.snapshot,
            self.io.is_present(),
        )
        .cleanup_io
            && let AttemptIoState::Ready(io) = &mut self.io
        {
            match io.cleanup().await {
                Ok(()) => self.io = AttemptIoState::Absent,
                Err(error) => failures.push(error),
            }
        }

        failures.into_result()
    }

    /// Waits for the client result of an interrupted mutating RPC.
    ///
    /// # Errors
    ///
    /// Returns a permanent internal error if the mutation worker stops before
    /// it reports the containerd result.
    pub(super) async fn settle_in_flight(&mut self) -> Result<(), ContainerEngineError> {
        let Some(mutation) = self.in_flight.as_ref() else {
            return Ok(());
        };
        let stage = mutation.stage;
        let result = self.join_active_mutation(stage).await?;
        self.apply_settled_mutation(stage, result)?;
        warn!(
            event = "containerd.mutation_settled_after_cancellation",
            resource_id = %self.resource_id,
            stage = stage.as_str(),
            "containerd cleanup resumed after an interrupted mutation completed",
        );
        Ok(())
    }

    /// Runs one bounded retry window for owned-resource cleanup.
    pub(super) async fn cleanup_owned_with_retry(&mut self) -> Result<(), ContainerEngineError> {
        let io_failure = match self.settle_io_preparation().await {
            Ok(()) => None,
            Err(error) if self.io.is_released() => {
                warn!(
                    event = "containerd.io_prepare_failed_after_cancellation",
                    resource_id = %self.resource_id,
                    error = %error,
                    "containerd cleanup released a failed I/O preparation",
                );
                None
            }
            Err(error) => Some(error),
        };
        if let Some(error) = io_failure.as_ref() {
            warn!(
                event = "containerd.io_ownership_lost",
                resource_id = %self.resource_id,
                error = %error,
                "containerd cleanup retained unresolved local I/O ownership",
            );
        }
        self.settle_in_flight().await?;
        let timeout = self.cleanup_timeout;
        let mut result = retry_cleanup(self, timeout, |attempt| {
            Box::pin(async move {
                attempt.settle_in_flight().await?;
                attempt.cleanup_owned().await
            })
        })
        .await;
        if result.is_ok() {
            result = match io_failure {
                Some(error) => Err(error),
                None if !self.is_released() => Err(ContainerEngineError::permanent(format!(
                    "containerd cleanup left unresolved ownership: {}",
                    self.unresolved_summary(),
                ))),
                None => Ok(()),
            };
        }
        if result.is_err() {
            self.abort_wait();
        }
        result
    }

    /// Returns `true` when no local or remote attempt resource remains owned.
    pub(super) fn is_released(&self) -> bool {
        !matches!(
            self.snapshot,
            Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
        ) && !matches!(
            self.container,
            Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
        ) && !matches!(
            self.task,
            Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
        ) && self.io.is_released()
            && self.wait.is_none()
            && self.in_flight.is_none()
    }

    /// Describes unresolved ownership for structured cleanup diagnostics.
    pub(super) fn unresolved_summary(&self) -> String {
        format!(
            "resource_id={}, snapshot={:?}, container={:?}, task={:?}, io={}, wait={}, mutation={}",
            self.resource_id,
            self.snapshot,
            self.container,
            self.task,
            self.io.status_label(),
            self.wait.is_some(),
            self.in_flight
                .as_ref()
                .map(|mutation| mutation.stage.as_str())
                .unwrap_or("none"),
        )
    }

    /// Starts and records a cancellation-safe remote mutation.
    fn begin_mutation(&mut self, stage: MutationStage, request: MutationRequest) {
        debug_assert!(self.in_flight.is_none());
        self.in_flight = Some(InFlightMutation {
            stage,
            owner: MutationOwner::Running(
                Arc::clone(&self.rpc).start_mutation(request, self.control_timeout),
            ),
        });
    }

    /// Waits for the active mutation and transfers its raw result.
    async fn await_mutation(
        &mut self,
        stage: MutationStage,
    ) -> Result<MutationResult, ContainerEngineError> {
        self.join_active_mutation(stage).await
    }

    /// Waits without removing the mutation owner before completion.
    async fn join_active_mutation(
        &mut self,
        stage: MutationStage,
    ) -> Result<MutationResult, ContainerEngineError> {
        let joined = {
            let mutation = self
                .in_flight
                .as_mut()
                .expect("mutation remains present until its result is observed");
            debug_assert_eq!(mutation.stage, stage);
            match &mut mutation.owner {
                MutationOwner::Running(owner) => owner.join().await,
                MutationOwner::Lost => {
                    return Err(ContainerEngineError::permanent(
                        "containerd mutation worker stopped before reporting its result",
                    ));
                }
            }
        };

        match joined {
            Ok(result) => {
                let mutation = self
                    .in_flight
                    .take()
                    .expect("mutation remains present until its result is observed");
                debug_assert_eq!(mutation.stage, stage);
                Ok(result)
            }
            Err(error) => {
                self.in_flight
                    .as_mut()
                    .expect("failed mutation remains recorded")
                    .owner = MutationOwner::Lost;
                Err(ContainerEngineError::permanent_from(
                    "containerd mutation worker stopped before reporting its result",
                    error,
                ))
            }
        }
    }

    /// Applies a mutation result that completed after caller cancellation.
    fn apply_settled_mutation(
        &mut self,
        stage: MutationStage,
        result: MutationResult,
    ) -> Result<(), ContainerEngineError> {
        match (stage, result) {
            (MutationStage::PrepareSnapshot, MutationResult::PrepareSnapshot(result)) => {
                self.snapshot = ownership_after_create_result(&result);
            }
            (MutationStage::CreateContainer, MutationResult::CreateContainer(result)) => {
                self.container = ownership_after_create_result(result.as_ref());
            }
            (MutationStage::CreateTask, MutationResult::CreateTask(result)) => {
                if let Ok(response) = &result {
                    self.task_pid = Some(response.pid);
                }
                self.task = ownership_after_create_result(&result);
            }
            (MutationStage::StartTask, MutationResult::StartTask(result)) => match result {
                Ok(_) => {
                    self.start_confirmed = true;
                    self.start_uncertain = false;
                }
                Err(status) => {
                    self.start_uncertain = ambiguous_create_status(&status);
                }
            },
            (MutationStage::KillTask, MutationResult::KillTask(result)) => match result {
                Ok(()) => {
                    self.termination_sent = true;
                    self.termination_requires_wait = true;
                }
                Err(status) if status.code() == Code::NotFound => {
                    self.task = Ownership::Absent;
                    self.abort_wait();
                }
                Err(status) if status.code() == Code::FailedPrecondition => {
                    self.termination_sent = true;
                    self.termination_requires_wait = self.start_confirmed;
                    if !self.termination_requires_wait {
                        self.abort_wait();
                    }
                }
                Err(_) => {}
            },
            (MutationStage::DeleteTask, MutationResult::DeleteTask(result)) => {
                self.task = ownership_after_delete_result(self.task, &result);
                if self.task == Ownership::Absent {
                    self.abort_wait();
                }
            }
            (MutationStage::DeleteContainer, MutationResult::DeleteContainer(result)) => {
                self.container = ownership_after_delete_result(self.container, &result);
            }
            (MutationStage::DeleteSnapshot, MutationResult::RemoveSnapshot(result)) => {
                self.snapshot = ownership_after_delete_result(self.snapshot, &result);
            }
            _ => {
                return Err(ContainerEngineError::permanent(
                    "containerd mutation returned a mismatched result",
                ));
            }
        }
        Ok(())
    }

    /// Aborts and releases an armed task wait.
    fn abort_wait(&mut self) {
        if let Some(mut wait) = self.wait.take() {
            wait.abort();
        }
    }
}

/// Active attempt that transfers unresolved ownership to deferred cleanup on drop.
struct ContainerdAttempt {
    /// Lifecycle state retained until explicit cleanup or deferred handoff.
    state: Option<AttemptState>,
    /// Pre-reserved finalizer admission owned for the full attempt lifetime.
    cleanup: Option<CleanupReservation>,
}

impl ContainerdAttempt {
    /// Creates an attempt with cleanup admission already reserved.
    fn new(state: AttemptState, cleanup: CleanupReservation) -> Self {
        Self {
            state: Some(state),
            cleanup: Some(cleanup),
        }
    }

    /// Returns mutable lifecycle state while the attempt is active.
    fn state_mut(&mut self) -> &mut AttemptState {
        self.state
            .as_mut()
            .expect("containerd attempt state exists until cleanup handoff")
    }

    /// Releases the reservation after complete explicit cleanup.
    fn disarm_if_released(&mut self) {
        if self.state.as_ref().is_some_and(AttemptState::is_released) {
            self.state.take();
            self.cleanup.take();
        }
    }

    /// Transfers unresolved state to the bounded cleanup owner.
    fn handoff(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let reservation = self
            .cleanup
            .take()
            .expect("cleanup admission exists while attempt state is owned");
        reservation.handoff(state);
    }
}

impl Drop for ContainerdAttempt {
    fn drop(&mut self) {
        self.handoff();
    }
}

#[async_trait]
impl ContainerAttempt for ContainerdAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        self.state_mut().io.ready_mut()?.take_stdout()
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        self.state_mut().io.ready_mut()?.take_stderr()
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.state_mut().start_inner().await
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.state_mut().wait_inner().await
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.state_mut().terminate_inner().await
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        let result = self.state_mut().cleanup_owned_with_retry().await;
        if result.is_ok() {
            self.disarm_if_released();
        }
        result
    }
}

/// Creates the error returned when one cleanup pass exceeds its window.
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

/// One borrowed cleanup attempt used by the bounded retry loop.
type CleanupOperation<'a> =
    Pin<Box<dyn Future<Output = Result<(), ContainerEngineError>> + Send + 'a>>;

/// Repeats a cleanup operation under one total representable deadline.
/// An overflowing duration waits without a local deadline.
async fn retry_cleanup<T, F>(
    state: &mut T,
    timeout: Duration,
    mut operation: F,
) -> Result<(), ContainerEngineError>
where
    T: Send,
    F: for<'a> FnMut(&'a mut T) -> CleanupOperation<'a>,
{
    let deadline = tokio::time::Instant::now().checked_add(timeout);
    let mut backoff = CLEANUP_BACKOFF_INITIAL;
    let mut last_error = None;

    loop {
        let result = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, operation(state)).await,
            None => Ok(operation(state).await),
        };
        match result {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) if error.class() == ContainerErrorClass::Permanent => {
                return Err(error);
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => return Err(cleanup_retry_exhausted(timeout, last_error)),
        }

        let now = tokio::time::Instant::now();
        if let Some(deadline) = deadline {
            if now >= deadline {
                return Err(cleanup_retry_exhausted(timeout, last_error));
            }
            tokio::time::sleep_until((now + backoff).min(deadline)).await;
        } else {
            tokio::time::sleep(backoff).await;
        }
        backoff = backoff.saturating_mul(2).min(CLEANUP_BACKOFF_MAX);
    }
}

/// Creates attempt identifiers unique to one engine session.
struct ResourceIdGenerator {
    /// Random process-local engine session.
    session: String,
    /// Monotonic sequence within the session.
    sequence: AtomicU64,
}

impl ResourceIdGenerator {
    /// Creates a generator with a random session.
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

    /// Creates a generator from fixed session bytes.
    fn from_session(session: [u8; SESSION_BYTES]) -> Self {
        Self {
            session: lower_hex(&session),
            sequence: AtomicU64::new(0),
        }
    }

    /// Returns the encoded engine session.
    fn session(&self) -> &str {
        &self.session
    }

    /// Returns the next metadata-safe attempt identifier.
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

/// Encodes bytes as lowercase hexadecimal.
fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Builds ownership labels for one attempt.
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

/// Returns whether every expected ownership label matches.
fn has_ownership_labels(
    actual: &HashMap<String, String>,
    expected: &HashMap<String, String>,
) -> bool {
    expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

/// Result of one ownership read-back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnershipReadBack {
    /// The resource is missing.
    Missing,
    /// The resource matches this attempt.
    Matching,
    /// The resource belongs to another owner.
    Mismatched,
    /// Read-back could not decide ownership.
    Unavailable,
}

/// Applies one read-back result to local ownership.
fn ownership_after_read_back(current: Ownership, outcome: OwnershipReadBack) -> Ownership {
    match outcome {
        OwnershipReadBack::Missing if current == Ownership::CreateUncertain => {
            Ownership::CreateUncertain
        }
        OwnershipReadBack::Missing => Ownership::Absent,
        OwnershipReadBack::Matching => Ownership::Owned,
        OwnershipReadBack::Mismatched => Ownership::Foreign,
        OwnershipReadBack::Unavailable if current == Ownership::Owned => Ownership::DeleteUncertain,
        OwnershipReadBack::Unavailable => current,
    }
}

/// Maps a completed create result to local ownership.
fn ownership_after_create_result<T>(result: &Result<T, Status>) -> Ownership {
    match result {
        Ok(_) => Ownership::Owned,
        Err(status) if status.code() == Code::AlreadyExists => Ownership::Foreign,
        Err(status) if ambiguous_create_status(status) => Ownership::CreateUncertain,
        Err(_) => Ownership::Absent,
    }
}

/// Maps a completed delete result to local ownership.
fn ownership_after_delete_result<T>(current: Ownership, result: &Result<T, Status>) -> Ownership {
    match result {
        Ok(_) => Ownership::DeleteUncertain,
        Err(status) if status.code() == Code::NotFound => Ownership::DeleteUncertain,
        Err(status) if ambiguous_create_status(status) => Ownership::DeleteUncertain,
        Err(_) => current,
    }
}

/// Checks snapshot identity against attempt metadata.
fn snapshot_identity_matches(
    actual_name: &str,
    actual_parent: &str,
    actual_labels: &HashMap<String, String>,
    expected_resource_id: &str,
    expected_parent: &str,
    expected_labels: &HashMap<String, String>,
) -> bool {
    actual_name == expected_resource_id
        && actual_parent == expected_parent
        && has_ownership_labels(actual_labels, expected_labels)
}

/// Checks container identity against attempt metadata.
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

/// Runtime task identity returned by containerd.
struct TaskIdentity<'a> {
    /// Container identifier reported for the process.
    container_id: &'a str,
    /// Process identifier reported by containerd.
    pid: u32,
    /// Standard output path reported by containerd.
    stdout: &'a str,
    /// Standard error path reported by containerd.
    stderr: &'a str,
}

/// Expected identity of one attempt runtime task.
struct ExpectedTaskIdentity<'a> {
    /// Attempt resource identifier.
    resource_id: &'a str,
    /// Process identifier when task creation returned one.
    pid: Option<u32>,
    /// Standard output path passed during task creation.
    stdout: Option<&'a str>,
    /// Standard error path passed during task creation.
    stderr: Option<&'a str>,
}

/// Checks runtime task identity against attempt metadata.
fn task_identity_matches(
    container: Ownership,
    actual: TaskIdentity<'_>,
    expected: ExpectedTaskIdentity<'_>,
) -> bool {
    container == Ownership::Owned
        && actual.container_id == expected.resource_id
        && expected.pid.is_none_or(|expected| actual.pid == expected)
        && expected
            .stdout
            .is_none_or(|expected| actual.stdout == expected)
        && expected
            .stderr
            .is_none_or(|expected| actual.stderr == expected)
}

/// Cleanup actions currently safe for one ownership state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CleanupEligibility {
    /// Whether task ownership requires read-back.
    confirm_task: bool,
    /// Whether the owned task may be deleted.
    delete_task: bool,
    /// Whether container ownership requires read-back.
    confirm_container: bool,
    /// Whether the owned container may be deleted.
    delete_container: bool,
    /// Whether snapshot ownership requires read-back.
    confirm_snapshot: bool,
    /// Whether the owned snapshot may be deleted.
    delete_snapshot: bool,
    /// Whether local output resources may be removed.
    cleanup_io: bool,
}

/// Computes dependency-safe cleanup actions.
fn cleanup_eligibility(
    task: Ownership,
    container: Ownership,
    snapshot: Ownership,
    has_io: bool,
) -> CleanupEligibility {
    let task_absent = task == Ownership::Absent;
    let task_released = matches!(task, Ownership::Absent | Ownership::Foreign);
    let container_absent = container == Ownership::Absent;

    CleanupEligibility {
        confirm_task: matches!(
            task,
            Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
        ),
        delete_task: task == Ownership::Owned,
        confirm_container: task_absent
            && matches!(
                container,
                Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
            ),
        delete_container: task_absent && container == Ownership::Owned,
        confirm_snapshot: task_absent
            && container_absent
            && matches!(
                snapshot,
                Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
            ),
        delete_snapshot: task_absent && container_absent && snapshot == Ownership::Owned,
        cleanup_io: task_released && has_io,
    }
}

/// Returns whether an RPC status leaves a mutation outcome uncertain.
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

/// Verifies the supported containerd major version.
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

/// Verifies that a discovered plugin reports ready state.
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

/// Returns whether a containerd platform matches the configured platform.
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

/// Formats an optional OCI platform variant.
fn platform_variant_suffix(variant: &str) -> String {
    if variant.is_empty() {
        String::new()
    } else {
        format!("/{variant}")
    }
}

/// Collects independent failures from one dependency-ordered cleanup attempt.
#[derive(Debug, Default)]
struct CleanupFailures(Vec<ContainerEngineError>);

impl CleanupFailures {
    /// Records one cleanup failure without stopping later independent steps.
    fn push(&mut self, error: ContainerEngineError) {
        self.0.push(error);
    }

    /// Returns one classified error containing every recorded failure.
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

/// Error source that preserves every failure from one cleanup attempt.
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

/// Combines the terminal results of both engine-owned domains.
fn combine_shutdown_results(
    cleanup: Result<(), ContainerEngineError>,
    io: Result<(), ContainerEngineError>,
) -> Result<(), ContainerEngineError> {
    match (cleanup, io) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(cleanup), Err(io)) => {
            let permanent = cleanup.class() == ContainerErrorClass::Permanent
                || io.class() == ContainerErrorClass::Permanent;
            let source = ShutdownFailureSet { cleanup, io };
            if permanent {
                Err(ContainerEngineError::permanent_from(
                    "containerd engine shutdown failed",
                    source,
                ))
            } else {
                Err(ContainerEngineError::retryable_from(
                    "containerd engine shutdown failed",
                    source,
                ))
            }
        }
    }
}

/// Errors returned by remote cleanup and local I/O shutdown.
#[derive(Debug)]
struct ShutdownFailureSet {
    /// Remote cleanup domain failure.
    cleanup: ContainerEngineError,
    /// Local I/O domain failure.
    io: ContainerEngineError,
}

impl fmt::Display for ShutdownFailureSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "remote cleanup: {}; local I/O: {}",
            self.cleanup, self.io,
        )
    }
}

impl Error for ShutdownFailureSet {}

/// Error source returned when one cleanup retry window expires.
#[derive(Debug)]
struct CleanupRetryExhausted {
    /// Configured retry window.
    timeout: Duration,
    /// Last observed cleanup failure.
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

/// Error source preserving creation and rollback failures.
#[derive(Debug)]
struct CreationRollbackFailure {
    /// Original attempt creation failure.
    creation: ContainerEngineError,
    /// Failure returned by immediate rollback.
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
#[path = "cancellation_tests.rs"]
pub(super) mod cancellation_tests;
#[cfg(test)]
#[path = "engine/tests.rs"]
mod tests;
