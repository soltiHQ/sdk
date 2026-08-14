//! Attempt-scoped containerd RPC access.
//!
//! The module keeps namespace metadata, client deadlines, and raw gRPC status
//! handling outside the attempt state machine. [`AttemptRpc`] is the private
//! seam used by production attempts and deterministic lifecycle tests.
//!
//! ```text
//! ContainerdAttempt
//!        |
//!        v
//!    AttemptRpc
//!        |
//!        v
//! ClientAttemptRpc -> containerd services
//! ```
//!
//! A task wait is returned only after its request has been polled once.
//! Cancellation during this handshake aborts the spawned wait task.

use std::{future::Future, pin::Pin, sync::Arc, task::Poll, time::Duration};

use async_trait::async_trait;
use containerd_client::{
    Client,
    services::v1::{
        CreateContainerRequest, CreateContainerResponse, CreateTaskRequest, CreateTaskResponse,
        DeleteContainerRequest, DeleteResponse, DeleteTaskRequest, GetContainerRequest,
        GetContainerResponse, GetRequest, GetResponse, KillRequest, StartRequest, StartResponse,
        WaitRequest, WaitResponse,
        snapshots::{
            MountsRequest, MountsResponse, PrepareSnapshotRequest, PrepareSnapshotResponse,
            RemoveSnapshotRequest, StatSnapshotRequest, StatSnapshotResponse,
        },
    },
    tonic::{
        GrpcMethod, Response, Status,
        client::Grpc,
        codegen::http::uri::PathAndQuery,
        metadata::{Ascii, MetadataValue},
    },
};
use tokio::{
    runtime::Handle,
    sync::oneshot,
    task::{JoinError, JoinHandle},
};

use super::image;
use crate::container::ContainerEngineError;

const WAIT_METHOD: &str = "/containerd.services.tasks.v1.Tasks/Wait";
const WAIT_SERVICE: &str = "containerd.services.tasks.v1.Tasks";

/// The decoded result produced by a task wait worker.
pub(super) type WaitRpcResult = Result<WaitResponse, Status>;

/// One mutating request that must outlive its caller's future.
pub(super) enum MutationRequest {
    /// Prepares an active snapshot.
    PrepareSnapshot(PrepareSnapshotRequest),
    /// Creates container metadata.
    CreateContainer(CreateContainerRequest),
    /// Creates a runtime task.
    CreateTask(CreateTaskRequest),
    /// Starts a runtime task.
    StartTask(StartRequest),
    /// Sends a signal to a runtime task.
    KillTask(KillRequest),
    /// Deletes a runtime task.
    DeleteTask(DeleteTaskRequest),
    /// Deletes container metadata.
    DeleteContainer(DeleteContainerRequest),
    /// Removes an active snapshot.
    RemoveSnapshot(RemoveSnapshotRequest),
}

/// Raw result from one completed mutating request.
pub(super) enum MutationResult {
    /// Snapshot preparation result.
    PrepareSnapshot(Result<PrepareSnapshotResponse, Status>),
    /// Container creation result.
    CreateContainer(Box<Result<CreateContainerResponse, Status>>),
    /// Runtime task creation result.
    CreateTask(Result<CreateTaskResponse, Status>),
    /// Runtime task start result.
    StartTask(Result<StartResponse, Status>),
    /// Runtime task signal result.
    KillTask(Result<(), Status>),
    /// Runtime task deletion result.
    DeleteTask(Result<DeleteResponse, Status>),
    /// Container deletion result.
    DeleteContainer(Result<(), Status>),
    /// Snapshot removal result.
    RemoveSnapshot(Result<(), Status>),
}

/// Owns a mutating client request until its result is observed.
pub(super) struct AttemptMutation {
    /// Task running on the engine cleanup runtime.
    task: AbortOnDrop<MutationResult>,
}

impl AttemptMutation {
    /// Creates a mutation from a spawned task.
    pub(super) fn from_task(task: JoinHandle<MutationResult>) -> Self {
        Self {
            task: AbortOnDrop::new(task),
        }
    }

    /// Waits for the client result without losing the task when cancelled.
    pub(super) async fn join(&mut self) -> Result<MutationResult, JoinError> {
        self.task.join().await
    }
}

/// Provides the attempt-scoped containerd operations used by the lifecycle state machine.
#[async_trait]
pub(super) trait AttemptRpc: Send + Sync + 'static {
    /// Starts a mutation on the engine-owned runtime.
    ///
    /// The returned owner retains the RPC after the calling lifecycle future
    /// is cancelled.
    fn start_mutation(
        self: Arc<Self>,
        request: MutationRequest,
        timeout: Duration,
    ) -> AttemptMutation {
        AttemptMutation::from_task(tokio::spawn(async move {
            match request {
                MutationRequest::PrepareSnapshot(request) => {
                    MutationResult::PrepareSnapshot(self.prepare_snapshot(request, timeout).await)
                }
                MutationRequest::CreateContainer(request) => MutationResult::CreateContainer(
                    Box::new(self.create_container(request, timeout).await),
                ),
                MutationRequest::CreateTask(request) => {
                    MutationResult::CreateTask(self.create_task(request, timeout).await)
                }
                MutationRequest::StartTask(request) => {
                    MutationResult::StartTask(self.start_task(request, timeout).await)
                }
                MutationRequest::KillTask(request) => {
                    MutationResult::KillTask(self.kill_task(request, timeout).await)
                }
                MutationRequest::DeleteTask(request) => {
                    MutationResult::DeleteTask(self.delete_task(request, timeout).await)
                }
                MutationRequest::DeleteContainer(request) => {
                    MutationResult::DeleteContainer(self.delete_container(request, timeout).await)
                }
                MutationRequest::RemoveSnapshot(request) => {
                    MutationResult::RemoveSnapshot(self.remove_snapshot(request, timeout).await)
                }
            }
        }))
    }

    /// Prepares an active snapshot.
    async fn prepare_snapshot(
        &self,
        request: PrepareSnapshotRequest,
        timeout: Duration,
    ) -> Result<PrepareSnapshotResponse, Status>;

    /// Reads snapshot ownership metadata.
    async fn stat_snapshot(
        &self,
        request: StatSnapshotRequest,
        timeout: Duration,
    ) -> Result<StatSnapshotResponse, Status>;

    /// Reads mounts for an active snapshot.
    async fn mount_snapshot(
        &self,
        request: MountsRequest,
        timeout: Duration,
    ) -> Result<MountsResponse, Status>;

    /// Removes an active snapshot.
    async fn remove_snapshot(
        &self,
        request: RemoveSnapshotRequest,
        timeout: Duration,
    ) -> Result<(), Status>;

    /// Creates container metadata.
    async fn create_container(
        &self,
        request: CreateContainerRequest,
        timeout: Duration,
    ) -> Result<CreateContainerResponse, Status>;

    /// Reads container ownership metadata.
    async fn get_container(
        &self,
        request: GetContainerRequest,
        timeout: Duration,
    ) -> Result<GetContainerResponse, Status>;

    /// Deletes container metadata.
    async fn delete_container(
        &self,
        request: DeleteContainerRequest,
        timeout: Duration,
    ) -> Result<(), Status>;

    /// Creates a runtime task.
    async fn create_task(
        &self,
        request: CreateTaskRequest,
        timeout: Duration,
    ) -> Result<CreateTaskResponse, Status>;

    /// Reads a runtime task.
    async fn get_task(&self, request: GetRequest, timeout: Duration)
    -> Result<GetResponse, Status>;

    /// Starts a runtime task.
    async fn start_task(
        &self,
        request: StartRequest,
        timeout: Duration,
    ) -> Result<StartResponse, Status>;

    /// Sends a signal to a runtime task.
    async fn kill_task(&self, request: KillRequest, timeout: Duration) -> Result<(), Status>;

    /// Deletes a runtime task.
    async fn delete_task(
        &self,
        request: DeleteTaskRequest,
        timeout: Duration,
    ) -> Result<DeleteResponse, Status>;

    /// Starts a task wait and returns after the request is armed.
    ///
    /// # Errors
    ///
    /// Returns an error when the service is unavailable, arming fails, or the
    /// wait worker stops before ownership can be transferred.
    async fn arm_wait(
        &self,
        request: WaitRequest,
        timeout: Duration,
    ) -> Result<AttemptWait, ContainerEngineError>;
}

/// Runs attempt RPCs through one containerd client and namespace.
#[derive(Clone)]
pub(super) struct ClientAttemptRpc {
    /// Shared client used to create service stubs.
    client: Arc<Client>,
    /// Namespace attached to every attempt request.
    namespace: MetadataValue<Ascii>,
    /// Runtime that retains mutating RPCs after caller cancellation.
    executor: Handle,
}

impl ClientAttemptRpc {
    /// Creates an attempt RPC adapter for one client and namespace.
    pub(super) fn new(
        client: Arc<Client>,
        namespace: MetadataValue<Ascii>,
        executor: Handle,
    ) -> Self {
        Self {
            client,
            namespace,
            executor,
        }
    }
}

#[async_trait]
impl AttemptRpc for ClientAttemptRpc {
    fn start_mutation(
        self: Arc<Self>,
        request: MutationRequest,
        timeout: Duration,
    ) -> AttemptMutation {
        let executor = self.executor.clone();
        AttemptMutation::from_task(executor.spawn(async move {
            match request {
                MutationRequest::PrepareSnapshot(request) => MutationResult::PrepareSnapshot(
                    raw_rpc(
                        timeout,
                        "containerd snapshot prepare failed",
                        self.client
                            .snapshots()
                            .prepare(image::namespaced_with_timeout(
                                request,
                                &self.namespace,
                                timeout,
                            )),
                    )
                    .await,
                ),
                MutationRequest::CreateContainer(request) => {
                    MutationResult::CreateContainer(Box::new(
                        raw_rpc(
                            timeout,
                            "containerd container create failed",
                            self.client
                                .containers()
                                .create(image::namespaced_with_timeout(
                                    request,
                                    &self.namespace,
                                    timeout,
                                )),
                        )
                        .await,
                    ))
                }
                MutationRequest::CreateTask(request) => MutationResult::CreateTask(
                    raw_rpc(
                        timeout,
                        "containerd task create failed",
                        self.client.tasks().create(image::namespaced_with_timeout(
                            request,
                            &self.namespace,
                            timeout,
                        )),
                    )
                    .await,
                ),
                MutationRequest::StartTask(request) => MutationResult::StartTask(
                    raw_rpc(
                        timeout,
                        "containerd task start failed",
                        self.client.tasks().start(image::namespaced_with_timeout(
                            request,
                            &self.namespace,
                            timeout,
                        )),
                    )
                    .await,
                ),
                MutationRequest::KillTask(request) => MutationResult::KillTask(
                    raw_rpc(
                        timeout,
                        "containerd task termination failed",
                        self.client.tasks().kill(image::namespaced_with_timeout(
                            request,
                            &self.namespace,
                            timeout,
                        )),
                    )
                    .await,
                ),
                MutationRequest::DeleteTask(request) => MutationResult::DeleteTask(
                    raw_rpc(
                        timeout,
                        "containerd task cleanup failed",
                        self.client.tasks().delete(image::namespaced_with_timeout(
                            request,
                            &self.namespace,
                            timeout,
                        )),
                    )
                    .await,
                ),
                MutationRequest::DeleteContainer(request) => MutationResult::DeleteContainer(
                    raw_rpc(
                        timeout,
                        "containerd container cleanup failed",
                        self.client
                            .containers()
                            .delete(image::namespaced_with_timeout(
                                request,
                                &self.namespace,
                                timeout,
                            )),
                    )
                    .await,
                ),
                MutationRequest::RemoveSnapshot(request) => MutationResult::RemoveSnapshot(
                    raw_rpc(
                        timeout,
                        "containerd snapshot cleanup failed",
                        self.client
                            .snapshots()
                            .remove(image::namespaced_with_timeout(
                                request,
                                &self.namespace,
                                timeout,
                            )),
                    )
                    .await,
                ),
            }
        }))
    }

    async fn prepare_snapshot(
        &self,
        request: PrepareSnapshotRequest,
        timeout: Duration,
    ) -> Result<PrepareSnapshotResponse, Status> {
        raw_rpc(
            timeout,
            "containerd snapshot prepare failed",
            self.client
                .snapshots()
                .prepare(image::namespaced_with_timeout(
                    request,
                    &self.namespace,
                    timeout,
                )),
        )
        .await
    }

    async fn stat_snapshot(
        &self,
        request: StatSnapshotRequest,
        timeout: Duration,
    ) -> Result<StatSnapshotResponse, Status> {
        raw_rpc(
            timeout,
            "containerd snapshot ownership read-back failed",
            self.client.snapshots().stat(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn mount_snapshot(
        &self,
        request: MountsRequest,
        timeout: Duration,
    ) -> Result<MountsResponse, Status> {
        raw_rpc(
            timeout,
            "containerd snapshot mounts lookup failed",
            self.client
                .snapshots()
                .mounts(image::namespaced_with_timeout(
                    request,
                    &self.namespace,
                    timeout,
                )),
        )
        .await
    }

    async fn remove_snapshot(
        &self,
        request: RemoveSnapshotRequest,
        timeout: Duration,
    ) -> Result<(), Status> {
        raw_rpc(
            timeout,
            "containerd snapshot cleanup failed",
            self.client
                .snapshots()
                .remove(image::namespaced_with_timeout(
                    request,
                    &self.namespace,
                    timeout,
                )),
        )
        .await
    }

    async fn create_container(
        &self,
        request: CreateContainerRequest,
        timeout: Duration,
    ) -> Result<CreateContainerResponse, Status> {
        raw_rpc(
            timeout,
            "containerd container create failed",
            self.client
                .containers()
                .create(image::namespaced_with_timeout(
                    request,
                    &self.namespace,
                    timeout,
                )),
        )
        .await
    }

    async fn get_container(
        &self,
        request: GetContainerRequest,
        timeout: Duration,
    ) -> Result<GetContainerResponse, Status> {
        raw_rpc(
            timeout,
            "containerd container ownership read-back failed",
            self.client.containers().get(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn delete_container(
        &self,
        request: DeleteContainerRequest,
        timeout: Duration,
    ) -> Result<(), Status> {
        raw_rpc(
            timeout,
            "containerd container cleanup failed",
            self.client
                .containers()
                .delete(image::namespaced_with_timeout(
                    request,
                    &self.namespace,
                    timeout,
                )),
        )
        .await
    }

    async fn create_task(
        &self,
        request: CreateTaskRequest,
        timeout: Duration,
    ) -> Result<CreateTaskResponse, Status> {
        raw_rpc(
            timeout,
            "containerd task create failed",
            self.client.tasks().create(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn get_task(
        &self,
        request: GetRequest,
        timeout: Duration,
    ) -> Result<GetResponse, Status> {
        raw_rpc(
            timeout,
            "containerd task ownership read-back failed",
            self.client.tasks().get(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn start_task(
        &self,
        request: StartRequest,
        timeout: Duration,
    ) -> Result<StartResponse, Status> {
        raw_rpc(
            timeout,
            "containerd task start failed",
            self.client.tasks().start(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn kill_task(&self, request: KillRequest, timeout: Duration) -> Result<(), Status> {
        raw_rpc(
            timeout,
            "containerd task termination failed",
            self.client.tasks().kill(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn delete_task(
        &self,
        request: DeleteTaskRequest,
        timeout: Duration,
    ) -> Result<DeleteResponse, Status> {
        raw_rpc(
            timeout,
            "containerd task cleanup failed",
            self.client.tasks().delete(image::namespaced_with_timeout(
                request,
                &self.namespace,
                timeout,
            )),
        )
        .await
    }

    async fn arm_wait(
        &self,
        request: WaitRequest,
        timeout: Duration,
    ) -> Result<AttemptWait, ContainerEngineError> {
        arm_wait(
            Arc::clone(&self.client),
            self.namespace.clone(),
            request,
            timeout,
        )
        .await
    }
}

/// Owns an armed task wait until the attempt consumes or aborts it.
pub(super) struct AttemptWait {
    /// Wait worker that is aborted when ownership is released early.
    task: AbortOnDrop<WaitRpcResult>,
}

impl AttemptWait {
    /// Spawns a controllable wait used by lifecycle tests.
    #[cfg(test)]
    pub(super) fn spawn<F>(future: F) -> Self
    where
        F: Future<Output = WaitRpcResult> + Send + 'static,
    {
        Self {
            task: AbortOnDrop::new(tokio::spawn(future)),
        }
    }

    /// Waits for the task result without losing the worker when this call is cancelled.
    pub(super) async fn join(&mut self) -> Result<WaitRpcResult, JoinError> {
        self.task.join().await
    }

    /// Stops the wait worker and releases its handle.
    pub(super) fn abort(&mut self) {
        self.task.abort();
    }
}

/// Aborts a spawned task unless its handle is explicitly transferred.
struct AbortOnDrop<T> {
    /// Spawned task retained until completion, abort, or handoff.
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    /// Takes ownership of a spawned task.
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// Transfers the task without aborting it.
    fn handoff(mut self) -> JoinHandle<T> {
        self.handle
            .take()
            .expect("abort-on-drop task is present until handoff")
    }

    /// Waits for the task while retaining ownership if the wait is cancelled.
    async fn join(&mut self) -> Result<T, JoinError> {
        let result = self
            .handle
            .as_mut()
            .expect("abort-on-drop task is present until completion")
            .await;
        self.handle = None;
        result
    }

    /// Aborts the task and releases its handle.
    fn abort(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Runs one namespaced unary RPC under the existing client deadline policy.
async fn raw_rpc<T, F>(timeout: Duration, reason: &'static str, future: F) -> Result<T, Status>
where
    F: Future<Output = Result<Response<T>, Status>>,
{
    image::raw_rpc_with_timeout(timeout, reason, future)
        .await
        .map(Response::into_inner)
}

/// Starts one raw wait request and transfers it after its first poll.
async fn arm_wait(
    client: Arc<Client>,
    namespace: MetadataValue<Ascii>,
    request: WaitRequest,
    timeout: Duration,
) -> Result<AttemptWait, ContainerEngineError> {
    let (armed_tx, armed_rx) = oneshot::channel();
    let wait = tokio::spawn(async move {
        let mut grpc = Grpc::new(client.channel());
        match tokio::time::timeout(timeout, grpc.ready()).await {
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

        let mut request = image::namespaced(request, &namespace);
        request
            .extensions_mut()
            .insert(GrpcMethod::new(WAIT_SERVICE, "Wait"));
        let mut request = Box::pin(grpc.unary(
            request,
            PathAndQuery::from_static(WAIT_METHOD),
            tonic_prost::ProstCodec::default(),
        ));
        poll_wait_once(&mut request, armed_tx)
            .await
            .map(Response::into_inner)
    });
    let mut wait = AbortOnDrop::new(wait);

    match armed_rx.await {
        Ok(Ok(())) => Ok(AttemptWait {
            task: AbortOnDrop::new(wait.handoff()),
        }),
        Ok(Err(status)) => {
            let _ = wait.join().await;
            Err(image::rpc_error("containerd task wait failed", status))
        }
        Err(error) => match wait.join().await {
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

/// Polls a wait request once before reporting that it is armed.
async fn poll_wait_once<F>(
    future: &mut Pin<Box<F>>,
    armed: oneshot::Sender<Result<(), Status>>,
) -> Result<Response<WaitResponse>, Status>
where
    F: Future<Output = Result<Response<WaitResponse>, Status>>,
{
    let mut armed = Some(armed);
    std::future::poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Ready(result) => {
            if let Some(armed) = armed.take() {
                let signal = result.as_ref().map(|_| ()).map_err(Status::clone);
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
