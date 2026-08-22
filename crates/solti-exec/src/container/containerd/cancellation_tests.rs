use std::{
    collections::HashMap,
    future::pending,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use containerd_client::{
    services::v1::{
        Container, CreateContainerRequest, CreateContainerResponse, CreateTaskRequest,
        CreateTaskResponse, DeleteContainerRequest, DeleteResponse, DeleteTaskRequest,
        GetContainerRequest, GetContainerResponse, GetRequest, GetResponse, KillRequest,
        StartRequest, StartResponse, WaitRequest, WaitResponse,
        snapshots::{
            Info, MountsRequest, MountsResponse, PrepareSnapshotRequest, PrepareSnapshotResponse,
            RemoveSnapshotRequest, StatSnapshotRequest, StatSnapshotResponse,
        },
    },
    tonic::Status,
    types::v1::Process,
};
use tokio::sync::Notify;

use super::*;
use crate::container::containerd::io_domain::TestPreparationCompleter;

const PARENT_SNAPSHOT: &str = "parent";
const RESOURCE_ID: &str = "attempt-1";
const SNAPSHOTTER: &str = "overlayfs";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    PrepareSnapshot,
    PrepareSnapshotAfterCancellation,
    PrepareSnapshotAmbiguousThenLateCommit,
    CreateContainer,
    CreateTask,
    ArmWait,
    StartTask,
    KillTask,
    DeleteTask,
    DeleteContainer,
    DeleteSnapshot,
}

struct Gate {
    operation: Operation,
    used: AtomicBool,
    entered: AtomicBool,
    entered_notify: Notify,
    release: Notify,
    completed: AtomicBool,
    completed_notify: Notify,
}

impl Gate {
    fn new(operation: Operation) -> Self {
        Self {
            operation,
            used: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            entered_notify: Notify::new(),
            release: Notify::new(),
            completed: AtomicBool::new(false),
            completed_notify: Notify::new(),
        }
    }

    async fn block_once(&self, operation: Operation) {
        if self.operation != operation || self.used.swap(true, Ordering::AcqRel) {
            return;
        }
        self.entered.store(true, Ordering::Release);
        self.entered_notify.notify_waiters();
        self.release.notified().await;
    }

    async fn wait_until_entered(&self) {
        loop {
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            self.entered_notify.notified().await;
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }

    fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
        self.completed_notify.notify_waiters();
    }

    async fn wait_until_completed(&self) {
        loop {
            if self.completed.load(Ordering::Acquire) {
                return;
            }
            self.completed_notify.notified().await;
        }
    }
}

#[derive(Default)]
struct RemoteLedger {
    snapshot: Option<SnapshotRecord>,
    snapshot_reads: usize,
    container: Option<Container>,
    task: Option<TaskRecord>,
    running: bool,
    task_status_override: Option<ProcessStatus>,
    deletes: Vec<Operation>,
}

struct SnapshotRecord {
    parent: String,
    labels: HashMap<String, String>,
}

struct TaskRecord {
    pid: u32,
    stdout: String,
    stderr: String,
}

struct FakeRpc {
    ledger: Arc<Mutex<RemoteLedger>>,
    gate: Option<Arc<Gate>>,
    wait_stopped: Arc<AtomicBool>,
    fail_task_delete_once: AtomicBool,
    fail_task_delete_precondition_once: AtomicBool,
    task_status_after_delete_precondition: Mutex<Option<ProcessStatus>>,
    task_delete_failed: AtomicBool,
    task_delete_failed_notify: Notify,
    ambiguous_start_without_commit: AtomicBool,
    commit_start_on_delete: AtomicBool,
    late_start_delete_is_unknown: AtomicBool,
}

impl FakeRpc {
    fn new(gated_operation: Option<Operation>) -> Arc<Self> {
        Arc::new(Self {
            ledger: Arc::new(Mutex::new(RemoteLedger::default())),
            gate: gated_operation.map(Gate::new).map(Arc::new),
            wait_stopped: Arc::new(AtomicBool::new(false)),
            fail_task_delete_once: AtomicBool::new(false),
            fail_task_delete_precondition_once: AtomicBool::new(false),
            task_status_after_delete_precondition: Mutex::new(None),
            task_delete_failed: AtomicBool::new(false),
            task_delete_failed_notify: Notify::new(),
            ambiguous_start_without_commit: AtomicBool::new(false),
            commit_start_on_delete: AtomicBool::new(false),
            late_start_delete_is_unknown: AtomicBool::new(false),
        })
    }

    fn gate(&self) -> Arc<Gate> {
        Arc::clone(self.gate.as_ref().expect("test operation must have a gate"))
    }

    fn assert_empty(&self) {
        let ledger = self
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        assert!(ledger.snapshot.is_none());
        assert!(ledger.container.is_none());
        assert!(ledger.task.is_none());
        assert!(!ledger.running);
    }

    fn install_foreign_resources(&self) {
        let mut ledger = self
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        ledger.snapshot = Some(SnapshotRecord {
            parent: "foreign-parent".to_owned(),
            labels: HashMap::from([("owner".to_owned(), "foreign".to_owned())]),
        });
        ledger.container = Some(Container {
            id: RESOURCE_ID.to_owned(),
            labels: HashMap::from([("owner".to_owned(), "foreign".to_owned())]),
            snapshotter: SNAPSHOTTER.to_owned(),
            snapshot_key: RESOURCE_ID.to_owned(),
            ..Default::default()
        });
        ledger.task = Some(TaskRecord {
            pid: 99,
            stdout: "/foreign/stdout".to_owned(),
            stderr: "/foreign/stderr".to_owned(),
        });
    }

    fn install_foreign_task(&self) {
        self.ledger
            .lock()
            .expect("remote ledger lock is not poisoned")
            .task = Some(TaskRecord {
            pid: 99,
            stdout: "/foreign/stdout".to_owned(),
            stderr: "/foreign/stderr".to_owned(),
        });
    }

    fn install_foreign_container(&self) {
        self.ledger
            .lock()
            .expect("remote ledger lock is not poisoned")
            .container = Some(Container {
            id: RESOURCE_ID.to_owned(),
            labels: HashMap::from([("owner".to_owned(), "foreign".to_owned())]),
            snapshotter: SNAPSHOTTER.to_owned(),
            snapshot_key: RESOURCE_ID.to_owned(),
            ..Default::default()
        });
    }

    fn install_foreign_snapshot(&self) {
        self.ledger
            .lock()
            .expect("remote ledger lock is not poisoned")
            .snapshot = Some(SnapshotRecord {
            parent: "foreign-parent".to_owned(),
            labels: test_labels(),
        });
    }

    async fn block(&self, operation: Operation) {
        if let Some(gate) = &self.gate {
            gate.block_once(operation).await;
        }
    }

    fn fail_next_task_delete(&self) {
        self.fail_task_delete_once.store(true, Ordering::Release);
    }

    fn fail_next_task_delete_with_precondition(&self) {
        self.fail_task_delete_precondition_once
            .store(true, Ordering::Release);
    }

    fn fail_next_task_delete_with_precondition_and_status(&self, status: ProcessStatus) {
        *self
            .task_status_after_delete_precondition
            .lock()
            .expect("task status override lock is not poisoned") = Some(status);
        self.fail_next_task_delete_with_precondition();
    }

    fn fail_start_ambiguously(&self) {
        self.ambiguous_start_without_commit
            .store(true, Ordering::Release);
    }

    fn commit_late_start_on_delete(&self) {
        self.commit_start_on_delete.store(true, Ordering::Release);
    }

    fn commit_late_start_on_delete_with_unknown(&self) {
        self.late_start_delete_is_unknown
            .store(true, Ordering::Release);
        self.commit_late_start_on_delete();
    }

    async fn wait_for_task_delete_failure(&self) {
        loop {
            if self.task_delete_failed.load(Ordering::Acquire) {
                return;
            }
            self.task_delete_failed_notify.notified().await;
        }
    }
}

struct WaitStopSignal(Arc<AtomicBool>);

impl Drop for WaitStopSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[async_trait]
impl AttemptRpc for FakeRpc {
    async fn prepare_snapshot(
        &self,
        request: PrepareSnapshotRequest,
        _timeout: Duration,
    ) -> Result<PrepareSnapshotResponse, Status> {
        if self
            .gate
            .as_ref()
            .is_some_and(|gate| gate.operation == Operation::PrepareSnapshotAfterCancellation)
        {
            let gate = self.gate();
            gate.block_once(Operation::PrepareSnapshotAfterCancellation)
                .await;
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.ledger
                .lock()
                .expect("remote ledger lock is not poisoned")
                .snapshot = Some(SnapshotRecord {
                parent: request.parent,
                labels: request.labels,
            });
            gate.mark_completed();
            return Ok(PrepareSnapshotResponse::default());
        }
        if self
            .gate
            .as_ref()
            .is_some_and(|gate| gate.operation == Operation::PrepareSnapshotAmbiguousThenLateCommit)
        {
            let gate = self.gate();
            let ledger = Arc::clone(&self.ledger);
            tokio::spawn(async move {
                gate.block_once(Operation::PrepareSnapshotAmbiguousThenLateCommit)
                    .await;
                ledger
                    .lock()
                    .expect("remote ledger lock is not poisoned")
                    .snapshot = Some(SnapshotRecord {
                    parent: request.parent,
                    labels: request.labels,
                });
                gate.mark_completed();
            });
            return Err(Status::deadline_exceeded(
                "snapshot outcome is not available to the client",
            ));
        }
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.snapshot.is_some() {
                return Err(Status::already_exists("snapshot exists"));
            }
            ledger.snapshot = Some(SnapshotRecord {
                parent: request.parent,
                labels: request.labels,
            });
        }
        self.block(Operation::PrepareSnapshot).await;
        Ok(PrepareSnapshotResponse::default())
    }

    async fn stat_snapshot(
        &self,
        _request: StatSnapshotRequest,
        _timeout: Duration,
    ) -> Result<StatSnapshotResponse, Status> {
        let mut ledger = self
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        ledger.snapshot_reads += 1;
        let snapshot = ledger
            .snapshot
            .as_ref()
            .ok_or_else(|| Status::not_found("snapshot is absent"))?;
        Ok(StatSnapshotResponse {
            info: Some(Info {
                name: RESOURCE_ID.to_owned(),
                parent: snapshot.parent.clone(),
                labels: snapshot.labels.clone(),
                ..Default::default()
            }),
        })
    }

    async fn mount_snapshot(
        &self,
        _request: MountsRequest,
        _timeout: Duration,
    ) -> Result<MountsResponse, Status> {
        Ok(MountsResponse::default())
    }

    async fn remove_snapshot(
        &self,
        _request: RemoveSnapshotRequest,
        _timeout: Duration,
    ) -> Result<(), Status> {
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.snapshot.take().is_none() {
                return Err(Status::not_found("snapshot is absent"));
            }
            ledger.deletes.push(Operation::DeleteSnapshot);
        }
        self.block(Operation::DeleteSnapshot).await;
        Ok(())
    }

    async fn create_container(
        &self,
        request: CreateContainerRequest,
        _timeout: Duration,
    ) -> Result<CreateContainerResponse, Status> {
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.container.is_some() {
                return Err(Status::already_exists("container exists"));
            }
            ledger.container = request.container;
        }
        self.block(Operation::CreateContainer).await;
        Ok(CreateContainerResponse::default())
    }

    async fn get_container(
        &self,
        _request: GetContainerRequest,
        _timeout: Duration,
    ) -> Result<GetContainerResponse, Status> {
        let ledger = self
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        Ok(GetContainerResponse {
            container: Some(
                ledger
                    .container
                    .clone()
                    .ok_or_else(|| Status::not_found("container is absent"))?,
            ),
        })
    }

    async fn delete_container(
        &self,
        _request: DeleteContainerRequest,
        _timeout: Duration,
    ) -> Result<(), Status> {
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.container.take().is_none() {
                return Err(Status::not_found("container is absent"));
            }
            ledger.deletes.push(Operation::DeleteContainer);
        }
        self.block(Operation::DeleteContainer).await;
        Ok(())
    }

    async fn create_task(
        &self,
        request: CreateTaskRequest,
        _timeout: Duration,
    ) -> Result<CreateTaskResponse, Status> {
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.task.is_some() {
                return Err(Status::already_exists("task exists"));
            }
            ledger.task = Some(TaskRecord {
                pid: 1,
                stdout: request.stdout,
                stderr: request.stderr,
            });
            ledger.task_status_override = None;
        }
        self.block(Operation::CreateTask).await;
        Ok(CreateTaskResponse {
            container_id: RESOURCE_ID.to_owned(),
            pid: 1,
        })
    }

    async fn get_task(
        &self,
        _request: GetRequest,
        _timeout: Duration,
    ) -> Result<GetResponse, Status> {
        let ledger = self
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        let task = ledger
            .task
            .as_ref()
            .ok_or_else(|| Status::not_found("task is absent"))?;
        Ok(GetResponse {
            process: Some(Process {
                id: RESOURCE_ID.to_owned(),
                pid: task.pid,
                stdout: task.stdout.clone(),
                stderr: task.stderr.clone(),
                status: ledger.task_status_override.unwrap_or(if ledger.running {
                    ProcessStatus::Running
                } else {
                    ProcessStatus::Created
                }) as i32,
                ..Default::default()
            }),
        })
    }

    async fn start_task(
        &self,
        _request: StartRequest,
        _timeout: Duration,
    ) -> Result<StartResponse, Status> {
        if self.ambiguous_start_without_commit.load(Ordering::Acquire) {
            return Err(Status::deadline_exceeded(
                "task start outcome is unavailable",
            ));
        }
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.task.is_none() {
                return Err(Status::not_found("task is absent"));
            }
            ledger.running = true;
            ledger.task_status_override = None;
        }
        self.block(Operation::StartTask).await;
        Ok(StartResponse { pid: 1 })
    }

    async fn kill_task(&self, _request: KillRequest, _timeout: Duration) -> Result<(), Status> {
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.task.is_none() {
                return Err(Status::not_found("task is absent"));
            }
            if !ledger.running {
                return Err(Status::failed_precondition("task is stopped"));
            }
            ledger.running = false;
            ledger.task_status_override = Some(ProcessStatus::Stopped);
        }
        self.block(Operation::KillTask).await;
        Ok(())
    }

    async fn delete_task(
        &self,
        _request: DeleteTaskRequest,
        _timeout: Duration,
    ) -> Result<DeleteResponse, Status> {
        if self
            .fail_task_delete_precondition_once
            .swap(false, Ordering::AcqRel)
        {
            if let Some(status) = self
                .task_status_after_delete_precondition
                .lock()
                .expect("task status override lock is not poisoned")
                .take()
            {
                let mut ledger = self
                    .ledger
                    .lock()
                    .expect("remote ledger lock is not poisoned");
                ledger.running = matches!(
                    status,
                    ProcessStatus::Running | ProcessStatus::Paused | ProcessStatus::Pausing
                );
                ledger.task_status_override = Some(status);
            }
            return Err(Status::failed_precondition("unrelated delete precondition"));
        }
        if self.fail_task_delete_once.swap(false, Ordering::AcqRel) {
            self.task_delete_failed.store(true, Ordering::Release);
            self.task_delete_failed_notify.notify_one();
            return Err(Status::unavailable("injected task delete failure"));
        }
        {
            let mut ledger = self
                .ledger
                .lock()
                .expect("remote ledger lock is not poisoned");
            if ledger.task.is_none() {
                return Err(Status::not_found("task is absent"));
            }
            let late_start = self.commit_start_on_delete.swap(false, Ordering::AcqRel);
            if late_start {
                ledger.running = true;
                ledger.task_status_override = None;
            }
            if ledger.running {
                if late_start
                    && self
                        .late_start_delete_is_unknown
                        .swap(false, Ordering::AcqRel)
                {
                    return Err(Status::unknown("cannot delete a running process"));
                }
                return Err(Status::failed_precondition("task is running"));
            }
            ledger.task = None;
            ledger.running = false;
            ledger.task_status_override = None;
            ledger.deletes.push(Operation::DeleteTask);
        }
        self.block(Operation::DeleteTask).await;
        Ok(DeleteResponse::default())
    }

    async fn arm_wait(
        &self,
        _request: WaitRequest,
        _timeout: Duration,
    ) -> Result<AttemptWait, ContainerEngineError> {
        let stopped = Arc::clone(&self.wait_stopped);
        let wait = if self
            .gate
            .as_ref()
            .is_some_and(|gate| gate.operation == Operation::ArmWait)
        {
            AttemptWait::spawn(async move {
                let _signal = WaitStopSignal(stopped);
                pending().await
            })
        } else {
            AttemptWait::spawn(async move {
                let _signal = WaitStopSignal(stopped);
                Ok(WaitResponse::default())
            })
        };
        if self
            .gate
            .as_ref()
            .is_some_and(|gate| gate.operation == Operation::ArmWait)
        {
            tokio::task::yield_now().await;
        }
        self.block(Operation::ArmWait).await;
        Ok(wait)
    }
}

fn test_labels() -> HashMap<String, String> {
    HashMap::from([
        (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
        (LABEL_SESSION.to_owned(), "test-session".to_owned()),
        (LABEL_RESOURCE_ID.to_owned(), RESOURCE_ID.to_owned()),
    ])
}

fn test_state(rpc: Arc<FakeRpc>) -> AttemptState {
    AttemptState::new(
        rpc,
        SNAPSHOTTER.to_owned(),
        RESOURCE_ID.to_owned(),
        test_labels(),
        AttemptIoState::Ready(ManagedAttemptIo::for_test(AttemptIo::for_test())),
        AttemptTimeouts {
            control: Duration::from_millis(20),
            cleanup: Duration::from_secs(1),
        },
    )
}

fn preparing_test_state(rpc: Arc<FakeRpc>) -> (AttemptState, TestPreparationCompleter) {
    let (preparation, result) = IoPreparation::controlled_for_test();
    (
        AttemptState::new(
            rpc,
            SNAPSHOTTER.to_owned(),
            RESOURCE_ID.to_owned(),
            test_labels(),
            AttemptIoState::Preparing(preparation),
            AttemptTimeouts {
                control: Duration::from_millis(20),
                cleanup: Duration::from_secs(1),
            },
        ),
        result,
    )
}

pub(in crate::container::containerd) fn test_state_for_cleanup_handoff() -> AttemptState {
    test_state(FakeRpc::new(None))
}

async fn create_all(state: &mut AttemptState) {
    state
        .create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new())
        .await
        .expect("test resources must be created");
}

async fn cancel_create_at(operation: Operation) {
    let rpc = FakeRpc::new(Some(operation));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    let mut create =
        Box::pin(state.create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new()));

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut create => panic!("creation completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("mutating RPC must reach its cancellation gate");
    drop(create);
    assert!(state.in_flight.is_some());
    gate.release();

    state
        .settle_in_flight()
        .await
        .expect("cancelled mutation must report its result");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("cancelled creation must remain cleanable");
    assert!(state.is_released());
    rpc.assert_empty();
}

async fn cancel_cleanup_at(operation: Operation) {
    let rpc = FakeRpc::new(Some(operation));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    let mut cleanup = Box::pin(state.cleanup_owned_with_retry());

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut cleanup => panic!("cleanup completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("delete RPC must reach its cancellation gate");
    drop(cleanup);
    assert!(state.in_flight.is_some());
    gate.release();

    state
        .settle_in_flight()
        .await
        .expect("cancelled mutation must report its result");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("cancelled cleanup must remain resumable");
    assert!(state.is_released());
    rpc.assert_empty();
}

async fn install_foreign_before_cleanup(operation: Operation) -> (AttemptState, Arc<FakeRpc>) {
    let rpc = FakeRpc::new(None);
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    match operation {
        Operation::DeleteTask => rpc.install_foreign_task(),
        Operation::DeleteContainer => rpc.install_foreign_container(),
        Operation::DeleteSnapshot => rpc.install_foreign_snapshot(),
        _ => panic!("replacement test requires a delete operation"),
    }
    let error = state
        .cleanup_owned_with_retry()
        .await
        .expect_err("foreign replacement must reject cleanup of the reused ID");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    (state, rpc)
}

#[tokio::test]
async fn cancelled_snapshot_create_is_cleaned_after_remote_commit() {
    cancel_create_at(Operation::PrepareSnapshot).await;
}

#[tokio::test]
async fn remote_commit_after_client_cancellation_is_cleaned() {
    let rpc = FakeRpc::new(Some(Operation::PrepareSnapshotAfterCancellation));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    let mut create =
        Box::pin(state.create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new()));

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut create => panic!("creation completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("snapshot mutation must reach its cancellation gate");
    drop(create);

    assert!(state.in_flight.is_some());
    assert!(
        rpc.ledger
            .lock()
            .expect("remote ledger lock is not poisoned")
            .snapshot
            .is_none()
    );

    gate.release();
    tokio::time::timeout(Duration::from_secs(1), gate.wait_until_completed())
        .await
        .expect("late snapshot commit must complete");
    assert!(state.in_flight.is_some());

    state
        .settle_in_flight()
        .await
        .expect("cancelled mutation must report its result");
    assert_eq!(state.snapshot, Ownership::Owned);
    state
        .cleanup_owned_with_retry()
        .await
        .expect("late snapshot commit must remain cleanable");
    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn cancelled_settlement_keeps_the_mutation_owner() {
    let rpc = FakeRpc::new(Some(Operation::PrepareSnapshotAfterCancellation));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    let mut create =
        Box::pin(state.create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new()));

    tokio::select! {
        () = gate.wait_until_entered() => {}
        result = &mut create => panic!("creation completed before cancellation: {result:?}"),
    }
    drop(create);

    let mut settlement = Box::pin(state.settle_in_flight());
    tokio::select! {
        result = &mut settlement => panic!("settlement completed before release: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    drop(settlement);
    assert!(state.in_flight.is_some());

    gate.release();
    state
        .settle_in_flight()
        .await
        .expect("second settlement must collect the retained mutation");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("settled ownership must remain cleanable");
    rpc.assert_empty();
}

#[tokio::test]
async fn cancelled_io_preparation_keeps_the_owner_in_attempt_state() {
    let rpc = FakeRpc::new(None);
    let (mut state, result) = preparing_test_state(rpc);
    let mut first_settlement = Box::pin(state.settle_io_preparation());

    tokio::select! {
        outcome = &mut first_settlement => {
            panic!("I/O preparation settled before its result: {outcome:?}")
        }
        () = tokio::task::yield_now() => {}
    }
    drop(first_settlement);
    assert!(matches!(state.io, AttemptIoState::Preparing(_)));

    result.ready();
    state
        .settle_io_preparation()
        .await
        .expect("second settlement must collect prepared I/O");
    assert!(matches!(state.io, AttemptIoState::Ready(_)));
    state
        .cleanup_owned_with_retry()
        .await
        .expect("prepared I/O must remain cleanable");
    assert!(state.is_released());
}

#[tokio::test]
async fn lost_mutation_result_stays_fail_closed() {
    let rpc = FakeRpc::new(None);
    let mut state = test_state(rpc);
    let task = tokio::spawn(std::future::pending::<MutationResult>());
    task.abort();
    state.snapshot = Ownership::CreateUncertain;
    state.in_flight = Some(InFlightMutation {
        stage: MutationStage::PrepareSnapshot,
        owner: MutationOwner::Running(AttemptMutation::from_task(task)),
    });

    let first = state
        .settle_in_flight()
        .await
        .expect_err("lost mutation result must fail permanently");
    assert_eq!(first.class(), ContainerErrorClass::Permanent);
    assert!(matches!(
        state.in_flight.as_ref().map(|mutation| &mutation.owner),
        Some(MutationOwner::Lost)
    ));

    let second = state
        .settle_in_flight()
        .await
        .expect_err("repeated settlement must remain fail closed");
    assert_eq!(second.class(), ContainerErrorClass::Permanent);
    assert!(!state.is_released());
}

#[tokio::test]
async fn ambiguous_snapshot_create_keeps_ownership_until_late_commit_is_cleaned() {
    let rpc = FakeRpc::new(Some(Operation::PrepareSnapshotAmbiguousThenLateCommit));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));

    let error = state
        .create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new())
        .await
        .expect_err("ambiguous snapshot creation must fail the attempt");
    assert_eq!(error.class(), ContainerErrorClass::Retryable);
    tokio::time::timeout(Duration::from_secs(1), gate.wait_until_entered())
        .await
        .expect("late commit worker must reach its gate");

    assert_eq!(state.snapshot, Ownership::CreateUncertain);
    assert!(!state.is_released());
    {
        let ledger = rpc
            .ledger
            .lock()
            .expect("remote ledger lock is not poisoned");
        assert!(ledger.snapshot.is_none());
        assert_eq!(ledger.snapshot_reads, 1);
    }

    gate.release();
    tokio::time::timeout(Duration::from_secs(1), gate.wait_until_completed())
        .await
        .expect("matching late snapshot commit must complete");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("late snapshot commit must be found and cleaned");
    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn cancelled_container_create_is_cleaned_after_remote_commit() {
    cancel_create_at(Operation::CreateContainer).await;
}

#[tokio::test]
async fn cancelled_task_create_is_cleaned_after_remote_commit() {
    cancel_create_at(Operation::CreateTask).await;
}

#[tokio::test]
async fn cancelled_wait_arming_aborts_wait_and_keeps_resources_cleanable() {
    let rpc = FakeRpc::new(Some(Operation::ArmWait));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    let mut create =
        Box::pin(state.create_resources("image", PARENT_SNAPSHOT, "runtime", Vec::new()));

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut create => panic!("creation completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("wait arming must reach its cancellation gate");
    drop(create);
    tokio::task::yield_now().await;

    assert!(rpc.wait_stopped.load(Ordering::Acquire));
    state
        .cleanup_owned_with_retry()
        .await
        .expect("resources must remain cleanable after wait arming cancellation");
    rpc.assert_empty();
}

#[tokio::test]
async fn cancelled_start_is_terminated_and_cleaned_after_remote_commit() {
    let rpc = FakeRpc::new(Some(Operation::StartTask));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    let mut start = Box::pin(state.start_inner());

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut start => panic!("start completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("start RPC must reach its cancellation gate");
    drop(start);
    assert!(state.in_flight.is_some());
    gate.release();

    state
        .settle_in_flight()
        .await
        .expect("cancelled mutation must report its result");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("cancelled start must remain cleanable");
    rpc.assert_empty();
}

#[tokio::test]
async fn ambiguous_uncommitted_start_remains_deletable_after_transient_delete_failure() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.fail_next_task_delete();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("created task must accept idempotent termination handling");
    assert!(!state.termination_sent);

    state
        .cleanup_owned_with_retry()
        .await
        .expect("owned created task must remain deletable on a later cleanup pass");

    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn late_start_after_failed_precondition_is_killed_on_cleanup_retry() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.commit_late_start_on_delete();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("pre-start termination is an idempotent cleanup step");
    assert!(!state.termination_sent);

    state
        .cleanup_owned_with_retry()
        .await
        .expect("late-started owned task must be killed and deleted on retry");

    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn late_start_after_unknown_delete_is_killed_on_cleanup_retry() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.commit_late_start_on_delete_with_unknown();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("pre-start termination is an idempotent cleanup step");
    assert!(!state.termination_sent);

    state
        .cleanup_owned_with_retry()
        .await
        .expect("an owned task rejected as running must be killed and deleted on retry");

    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn stopped_readback_after_ambiguous_start_is_reaped_and_deleted() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.fail_next_task_delete_with_precondition_and_status(ProcessStatus::Stopped);
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("pre-start termination is an idempotent cleanup step");
    assert!(!state.termination_sent);

    state
        .cleanup_owned_with_retry()
        .await
        .expect("a matching stopped task must be reaped and deleted on cleanup retry");

    assert!(state.start_confirmed);
    assert!(!state.start_uncertain);
    assert!(state.exit_status.is_some());
    assert!(state.is_released());
    rpc.assert_empty();
}

#[tokio::test]
async fn unknown_readback_after_ambiguous_start_remains_unconfirmed_and_fail_closed() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.fail_next_task_delete_with_precondition_and_status(ProcessStatus::Unknown);
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("pre-start termination is an idempotent cleanup step");
    assert!(!state.termination_sent);

    let error = state
        .cleanup_owned_with_retry()
        .await
        .expect_err("an unknown task state must not be reclassified as a late start");

    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd attempt cleanup failed");
    assert!(!state.start_confirmed);
    assert!(state.start_uncertain);
    assert!(!state.termination_sent);
    assert_eq!(state.task, Ownership::Owned);
    let ledger = rpc
        .ledger
        .lock()
        .expect("remote ledger lock is not poisoned");
    assert!(ledger.task.is_some());
    assert!(!ledger.running);
}

#[tokio::test]
async fn unrelated_delete_precondition_is_not_reclassified_as_retryable() {
    let rpc = FakeRpc::new(None);
    rpc.fail_start_ambiguously();
    rpc.fail_next_task_delete_with_precondition();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;

    let start = state
        .start_inner()
        .await
        .expect_err("ambiguous start must fail the active attempt");
    assert_eq!(start.class(), ContainerErrorClass::Retryable);
    state
        .terminate_inner()
        .await
        .expect("pre-start termination is an idempotent cleanup step");

    let error = state
        .cleanup_owned_with_retry()
        .await
        .expect_err("an unrelated delete precondition must remain permanent");

    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd attempt cleanup failed");
    assert_eq!(state.task, Ownership::Owned);
}

#[tokio::test]
async fn cancelled_termination_is_settled_before_cleanup() {
    let rpc = FakeRpc::new(Some(Operation::KillTask));
    let gate = rpc.gate();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    state
        .start_inner()
        .await
        .expect("test task must start before termination");
    let mut terminate = Box::pin(state.terminate_inner());

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = gate.wait_until_entered() => {}
            result = &mut terminate => panic!("termination completed before cancellation: {result:?}"),
        }
    })
    .await
    .expect("kill RPC must reach its cancellation gate");
    drop(terminate);
    assert!(state.in_flight.is_some());
    gate.release();

    state
        .settle_in_flight()
        .await
        .expect("cancelled termination must report its result");
    state
        .cleanup_owned_with_retry()
        .await
        .expect("cancelled termination must remain cleanable");
    rpc.assert_empty();
}

#[tokio::test]
async fn cancelled_task_delete_is_resumed() {
    cancel_cleanup_at(Operation::DeleteTask).await;
}

#[tokio::test]
async fn cancelled_container_delete_is_resumed() {
    cancel_cleanup_at(Operation::DeleteContainer).await;
}

#[tokio::test]
async fn cancelled_snapshot_delete_is_resumed() {
    cancel_cleanup_at(Operation::DeleteSnapshot).await;
}

#[tokio::test]
async fn cleanup_cancelled_during_retry_sleep_remains_resumable() {
    let rpc = FakeRpc::new(None);
    rpc.fail_next_task_delete();
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    let mut cleanup = Box::pin(state.cleanup_owned_with_retry());

    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = rpc.wait_for_task_delete_failure() => {}
            result = &mut cleanup => panic!("cleanup completed before retry cancellation: {result:?}"),
        }
    })
    .await
    .expect("cleanup must enter retry backoff");
    drop(cleanup);

    state
        .cleanup_owned_with_retry()
        .await
        .expect("cleanup must resume after retry sleep cancellation");
    rpc.assert_empty();
}

#[tokio::test]
async fn foreign_task_replacement_before_cleanup_is_not_deleted() {
    let (state, rpc) = install_foreign_before_cleanup(Operation::DeleteTask).await;
    assert_eq!(state.task, Ownership::Foreign);
    let ledger = rpc
        .ledger
        .lock()
        .expect("remote ledger lock is not poisoned");
    assert!(ledger.task.is_some());
    assert_eq!(
        ledger
            .deletes
            .iter()
            .filter(|operation| **operation == Operation::DeleteTask)
            .count(),
        0,
    );
}

#[tokio::test]
async fn foreign_container_replacement_before_cleanup_is_not_deleted() {
    let (state, rpc) = install_foreign_before_cleanup(Operation::DeleteContainer).await;
    assert_eq!(state.container, Ownership::Foreign);
    let ledger = rpc
        .ledger
        .lock()
        .expect("remote ledger lock is not poisoned");
    assert!(ledger.container.is_some());
    assert_eq!(
        ledger
            .deletes
            .iter()
            .filter(|operation| **operation == Operation::DeleteContainer)
            .count(),
        0,
    );
}

#[tokio::test]
async fn foreign_snapshot_replacement_before_cleanup_is_not_deleted() {
    let (state, rpc) = install_foreign_before_cleanup(Operation::DeleteSnapshot).await;
    assert_eq!(state.snapshot, Ownership::Foreign);
    let ledger = rpc
        .ledger
        .lock()
        .expect("remote ledger lock is not poisoned");
    assert!(ledger.snapshot.is_some());
    assert_eq!(
        ledger
            .deletes
            .iter()
            .filter(|operation| **operation == Operation::DeleteSnapshot)
            .count(),
        0,
    );
}

#[tokio::test]
async fn foreign_resources_are_never_deleted() {
    let rpc = FakeRpc::new(None);
    rpc.install_foreign_resources();
    let mut state = test_state(Arc::clone(&rpc));
    state.snapshot = Ownership::Foreign;
    state.container = Ownership::Foreign;
    state.task = Ownership::Foreign;

    state
        .cleanup_owned_with_retry()
        .await
        .expect("foreign ownership must be released without remote deletion");

    let ledger = rpc
        .ledger
        .lock()
        .expect("remote ledger lock is not poisoned");
    assert!(ledger.snapshot.is_some());
    assert!(ledger.container.is_some());
    assert!(ledger.task.is_some());
    assert!(ledger.deletes.is_empty());
}

#[tokio::test]
async fn explicit_cleanup_disarms_deferred_submission() {
    let rpc = FakeRpc::new(None);
    let mut state = test_state(Arc::clone(&rpc));
    create_all(&mut state).await;
    let mut observed = crate::container::containerd::cleanup::tests::observed_test_domain(1);
    let reservation = observed
        .domain()
        .try_reserve()
        .expect("cleanup slot must be available");
    let mut attempt = ContainerdAttempt::new(state, reservation);

    attempt
        .cleanup()
        .await
        .expect("explicit cleanup must release attempt ownership");
    drop(attempt);

    observed.assert_no_handoff();
    observed
        .domain()
        .try_reserve()
        .expect("explicit cleanup must release admission");
    rpc.assert_empty();
}
