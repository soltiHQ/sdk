# solti-exec source guide

This document is a reading map for contributors.

It shows which module owns each execution boundary and how one workload attempt is created and cleaned up.
The Rust source and its module-level documentation remain the source of truth.

## Crate map

`lib.rs` exposes a feature-gated public API.
The internal modules separate policy values, native enforcement, runner orchestration, and engine adapters.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}, "themeVariables": {"fontSize": "12px"}}}%%
flowchart TB
    Public["lib.rs<br/>feature-gated public API"]
    Isolation["isolation/<br/>shared process values"]
    Host["host/<br/>host process controls"]
    Subprocess["subprocess/<br/>subprocess runner"]
    Container["container/<br/>engine-neutral runner"]
    Containerd["container/containerd/<br/>native containerd 2.x"]
    Oci["container/oci.rs<br/>OCI policy translation"]
    Output["output.rs<br/>bounded line pipeline"]
    Registration["registration.rs<br/>runner naming"]
    Error["error.rs<br/>public setup errors"]

    Public --> Isolation
    Public --> Host
    Public --> Subprocess
    Public --> Container
    Public --> Error

    Host --> Isolation
    Subprocess --> Host
    Subprocess --> Output
    Subprocess --> Registration
    Container --> Isolation
    Container --> Output
    Container --> Registration
    Containerd --> Container
    Containerd --> Oci
```

The arrows show direct use.
They do not represent runtime ownership.

| Module                   | Owns                                                                 | Does not own                                     |
|--------------------------|----------------------------------------------------------------------|--------------------------------------------------|
| `lib.rs`                 | Feature-gated exports and crate-level documentation                  | Routing, scheduling, or task state               |
| `isolation/`             | Shared capability, credential, resource, and seccomp values          | Native enforcement                               |
| `host/`                  | Host policy preparation, child hooks, cgroups, and process domains   | Workload resolution or process supervision       |
| `subprocess/backend.rs`  | Environment, cwd, output, script, descriptor, and host settings      | Attempt lifecycle                                |
| `subprocess/task.rs`     | Immutable settings resolved for one reusable Taskvisor task          | Attempt-scoped operating-system resources        |
| `subprocess/boundary.rs` | Pinned working-directory handles                                     | Filesystem confinement after process start       |
| `subprocess/script.rs`   | Attempt-scoped script transport                                      | Interpreter selection                            |
| `subprocess/runner.rs`   | Workload conversion and subprocess attempt orchestration             | Router selection or restart policy               |
| `subprocess/domain.rs`   | Child, process-group identity, host domain, reap, and drop finalizer | Task result policy                               |
| `container/engine.rs`    | Engine and attempt contracts                                         | A concrete engine protocol                       |
| `container/runner.rs`    | Workload conversion and engine-neutral attempt orchestration         | Image, snapshot, or runtime implementation       |
| `container/policy.rs`    | Engine-neutral container process policy                              | OCI or host-process enforcement                  |
| `container/oci.rs`       | Translation of container process policy into OCI fields              | Base OCI specification                           |
| `container/containerd/`  | Containerd probe, image, spec, attempt I/O, ownership, and cleanup   | Daemon lifecycle, CRI, CNI, or foreign resources |
| `output.rs`              | Bounded line decoding, tracing, and `OutputSink` publication         | Output retention or subscriptions                |
| `registration.rs`        | Runner-name validation and the standard runner label                 | GVK and selector routing                         |
| `error.rs`               | Registration and backend-construction errors                         | Attempt failure policy                           |

## Feature boundaries

No feature is enabled by default.
Each execution path is opt-in.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    HostProcess["host-process"]
    Subprocess["subprocess"]
    Seccomp["seccomp"]
    Container["container"]
    Containerd["containerd"]

    Subprocess --> HostProcess
    Seccomp --> HostProcess
    Containerd --> Container
```

| Feature        | Exposes                                            | Platform boundary                                      |
|----------------|----------------------------------------------------|--------------------------------------------------------|
| `host-process` | Host policy and low-level process controls         | Unix and Linux controls are validated before use       |
| `subprocess`   | `SubprocessRunner`                                 | Uses the strongest available host process boundary     |
| `seccomp`      | Host seccomp enforcement                           | The BPF renderer is available on Linux                 |
| `container`    | `ContainerRunner` and engine contracts             | The contract does not select a platform or engine      |
| `containerd`   | Native containerd 2.x adapter                      | Container execution and attempt FIFOs require Linux    |

`container` does not imply `containerd`.
A final binary may provide another `ContainerEngine` implementation.

`containerd` compiles outside Linux.
Attempt I/O returns an unsupported-platform error there.

## Build and attempt boundaries

Both runners build reusable `TaskRef` values.
Taskvisor owns execution after the build.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Task["solti_model::Task"]
    Router["RunnerRouter<br/>GVK + selector"]
    Build["Runner::build_task"]
    Config["Immutable task settings"]
    TaskRef["taskvisor::TaskRef"]
    Spawn["Taskvisor attempt"]
    Resources["Fresh attempt resources"]
    Result["Task result + cleanup"]

    Task --> Router
    Router --> Build
    Build --> Config
    Config --> TaskRef
    TaskRef --> Spawn
    Spawn --> Resources
    Resources --> Result
```

The build checks the exact built-in workload GVK.
It merges task and runner environment values.
It does not spawn a process or call a container engine.

The subprocess build decodes script bodies and pins an explicit cwd.
The container build stores image overrides and process policy.

Every Taskvisor attempt receives fresh runtime resources.
The same `TaskRef` can therefore run more than once.

## Isolation translation

`isolation/` defines values shared by process-based backends.
It does not apply them.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Values["isolation/<br/>rlimits, resources, identity,<br/>capabilities, seccomp"]
    HostPolicy["HostProcessPolicy"]
    ContainerPolicy["ContainerProcessPolicy"]
    HostNative["pre_exec hooks<br/>cgroup v2 handles"]
    Oci["OCI process<br/>OCI Linux resources"]

    Values --> HostPolicy
    Values --> ContainerPolicy
    HostPolicy --> HostNative
    ContainerPolicy --> Oci
```

`HostProcessPolicy` and `ContainerProcessPolicy` are separate policies.
They reuse value types but have different enforcement semantics.

| Shared value         | Host translation                                      | Container translation                                  |
|----------------------|-------------------------------------------------------|--------------------------------------------------------|
| `RlimitConfig`       | Clamped against inherited hard limits before spawn    | Replaces selected OCI rlimit entries                   |
| `CgroupLimits`       | Creates and owns one cgroup v2 child per attempt       | Updates selected OCI Linux resource fields             |
| `ProcessCredentials` | Applies exact IDs between `fork` and `execve`          | Replaces the OCI process user                          |
| `LinuxCapability`    | Reduces the host capability boundary                  | Replaces all five OCI capability sets                  |
| `SeccompPolicy`      | Installs a host BPF filter                             | Builds OCI named syscall rules                         |

The host seccomp renderer also rejects the x32 syscall ABI on LP64 `x86_64`.
OCI named syscall rules cannot express that raw-number guard.

An empty host policy adds no host control.
An empty container policy preserves the engine's base OCI specification.
Configured controls are validated before enforcement.

## Host process preparation

Host controls use a two-stage preparation boundary.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Policy["HostProcessPolicy"]
    Prepare["prepare<br/>validate platform and values<br/>pin cgroup parent"]
    Prepared["PreparedHostProcessPolicy"]
    Attempt["prepare_attempt<br/>create and pin attempt cgroup"]
    Token["PreparedHostProcessAttempt"]
    Command["apply_to_command"]
    Hooks["process state → rlimits → cgroup → security"]
    Domain["AttemptProcessDomain"]

    Policy --> Prepare
    Prepare --> Prepared
    Prepared --> Attempt
    Attempt --> Token
    Token --> Command
    Command --> Hooks
    Command --> Domain
```

Runner construction validates platform support and static values.
It resolves the inherited or explicit cgroup parent.

Attempt preparation creates the child cgroup before process creation.
The returned token proves that configured attempt resources were prepared.

`apply_to_command` installs child hooks and returns the owning process domain.
A configured hook failure prevents `execve`.

The backend owns its process-specific termination boundary.
`AttemptProcessDomain` owns only the optional cgroup boundary.
It removes that cgroup only after `cgroup.events` reports `populated 0`.

These controls harden host processes.
They do not form a complete sandbox for untrusted code.

## Subprocess lifecycle

`SubprocessRunner` separates runner construction, task build, and attempt execution.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}, "themeVariables": {"fontSize": "12px"}}}%%
flowchart TB
    Runner["Runner construction<br/>prepare backend<br/>start bounded finalizer"]
    Build["Task build<br/>resolve mode and environment<br/>pin cwd"]
    TaskRef["Reusable TaskRef"]
    Attempt["Attempt<br/>reserve cleanup ownership<br/>prepare host resources"]
    Script{"Script mode?"}
    Materialize["Create attempt script transport"]
    Spawn["Attach fd and host boundaries<br/>spawn child"]
    Observe{"Exit or cancellation?"}
    ExitDrain["Bounded output grace"]
    Terminate["Terminate cgroup, process group, and leader"]
    Reap["Reap leader"]
    FinishOutput["Finish or abort output readers"]
    Cleanup["Clean cgroup and script transport"]

    Runner --> Build
    Build --> TaskRef
    TaskRef --> Attempt
    Attempt --> Script
    Script -->|yes| Materialize
    Script -->|no| Spawn
    Materialize --> Spawn
    Spawn --> Observe
    Observe -->|leader exit| ExitDrain
    Observe -->|cancellation| Terminate
    ExitDrain --> Terminate
    Terminate --> Reap
    Reap --> FinishOutput
    FinishOutput --> Cleanup
```

Command mode starts the configured executable directly.
Script mode stores decoded bytes in fresh attempt-scoped transport.

Linux uses a sealed anonymous `memfd`.
Other Unix platforms use an unlinked file.
Non-Unix platforms use a named temporary file.

Unix attempts own a session and process group.
A configured cgroup is an additional boundary.
Termination requests every available boundary before the leader is reaped.

Normal leader exit receives the same descendant cleanup.
This prevents descendants from retaining output pipes or attempt resources.

Lifecycle termination, reap, and cleanup errors are fatal.
Exit status is evaluated only after lifecycle cleanup succeeds.

## Dropped subprocess attempts

`ActiveProcessDomain` owns the child wait status.
It keeps the Unix process-group identity reserved until termination.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Active["ActiveProcessDomain"]
    Drop["Task future dropped"]
    Request["Synchronous termination request"]
    Worker["Tokio-independent finalizer"]
    Reap["Reap child"]
    Cleanup["Retry host cleanup"]

    Active --> Drop
    Drop --> Request
    Request --> Worker
    Worker --> Reap
    Reap --> Cleanup
```

Drop does not wait for process exit.
Each runner owns one finalizer with 1024 ownership slots by default.
An attempt reserves one slot before script transport, cgroup, or process creation.
It transfers the child, host domain, process-group state, and reservation to the
finalizer worker.

Cancellation after cgroup preparation and process spawn failure transfer the
prepared host domain through the same queue. The queue is finite because every
entry already owns one reservation. Handoff uses a non-blocking send.

The worker retains the child until it reaps it or detects lost wait ownership.
It then advances host cleanup outside the attempt's Tokio runtime.
An operation enters quarantine after ten persistent operating-system errors.
Quarantine retains its ownership slot and closes new admission.

After all supervisors and task references have stopped,
`SubprocessRunner::shutdown` closes admission, waits for accepted ownership,
and joins the worker. A cancelled or timed-out shutdown leaves that terminal
state available to another shutdown call.

The embedding process must not reap arbitrary children.
It must not enable automatic `SIGCHLD` reaping.

## Container engine boundary

`ContainerRunner` owns the engine-neutral lifecycle.
`ContainerEngine` owns engine-specific creation.
`ContainerEngineBinding` records the engine provider's drop-ownership declaration.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Build["ContainerRunner::build_task"]
    Config["Immutable task settings"]
    Spawn["Taskvisor attempt"]
    Request["ContainerRequest"]
    Engine["ContainerEngine::create_attempt"]
    Attempt["Stopped ContainerAttempt<br/>wait already armed"]
    Output["Take stdout and stderr"]
    Start["start"]
    Observe{"wait returns or cancel?"}
    Terminate["terminate"]
    WaitAfter["wait after termination"]
    Cleanup["cleanup"]

    Build --> Config
    Config --> Spawn
    Spawn --> Request
    Request --> Engine
    Engine --> Attempt
    Attempt --> Output
    Output --> Start
    Start --> Observe
    Observe -->|wait returns| Cleanup
    Observe -->|cancel| Terminate
    Terminate --> WaitAfter
    WaitAfter --> Cleanup
```

`probe` is explicit.
The runner never calls it.

`create_attempt` returns a stopped attempt.
Exit observation must be armed before the attempt is returned.

Custom engine registration requires one typed ownership declaration:

| Contract | Provider declaration |
|----------|----------------------|
| `DropReleases` | Dropping in-flight creation or the returned attempt synchronously releases all accepted ownership and leaves no lifecycle task |
| `PreAdmittedFinalizer` | Finite finalizer admission precedes the first attempt-owned resource or mutation; drop transfers confirmed and uncertain attempt ownership without starting unbounded work |

The binding makes the declaration explicit in integration source.
It does not verify a custom engine implementation at runtime.
The native containerd conversion selects `PreAdmittedFinalizer` because its
admission, drop handoff, failure state, and shutdown path are implemented in the adapter.

The runner polls creation and attempt methods inline.
It does not spawn engine lifecycle futures.
Cooperative cancellation waits for termination, exit observation, and cleanup.
A Taskvisor timeout or force-abort drops the current lifecycle future and the attempt object.
The engine's declared contract owns any cleanup that must continue after that drop.
The only runner-spawned work in this path is at most two output readers.
Their handles abort on drop.

`terminate` and `cleanup` are idempotent.
Cleanup may remove only attempt-owned resources.
Completed cleanup steps remain completed across retries.

Retryable create, start, and wait errors become retryable Taskvisor failures.
Permanent errors become fatal Taskvisor failures.
An error returned by create wins over cancellation observed after create returns.
Termination and cleanup errors are always fatal.

## Native containerd lifecycle

`ContainerdEngine` implements the generic boundary against native containerd 2.x services.
It uses one explicit Unix socket and namespace.

Connection probes:

- containerd major version 2;
- the configured snapshotter;
- the selected platform;
- the configured OCI runtime.

The adapter never scans for sockets or starts a daemon.
It does not use CRI or configure CNI.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}, "themeVariables": {"fontSize": "12px"}}}%%
flowchart TB
    Request["ContainerRequest"]
    Admission["Reserve lifecycle<br/>cleanup admission"]
    Image["Pull and unpack image<br/>read verified metadata"]
    Spec["Build base OCI spec<br/>apply process policy"]
    Io["Create private attempt FIFOs"]
    Snapshot["Prepare snapshot"]
    Container["Create container"]
    Task["Create stopped task"]
    Arm["Activate output<br/>arm Wait"]
    Attempt["Return ContainerdAttempt"]
    Start["Start task"]
    Wait["Wait without workload deadline"]
    Cleanup["Delete task → container → snapshot<br/>remove local I/O"]
    Rollback["Ownership-aware rollback"]

    Request --> Admission
    Admission --> Image
    Image --> Spec
    Spec --> Io
    Io --> Snapshot
    Snapshot --> Container
    Container --> Task
    Task --> Arm
    Arm --> Attempt
    Attempt --> Start
    Start --> Wait
    Wait --> Cleanup

    Snapshot -->|create failure| Rollback
    Container -->|create failure| Rollback
    Task -->|create failure| Rollback
```

Image transfer has a separate 10-minute deadline by default.
Control and metadata RPCs use a 30-second deadline by default.
Every request carries a finite gRPC deadline. Local futures also enforce the
default deadlines and configured deadlines that fit Tokio's `Instant` range.
Lifecycle admission is reserved before image resolution. The reservation bounds
the client-side resolve future and stays charged through the returned attempt or
deferred cleanup. Image resolution and unpack use containerd's shared image
store. They are not stored in `AttemptState` and a dropped create future does
not hand their transfer to the cleanup domain.

Content reads share one control deadline across the complete stream.
Each image metadata object is limited to 4 MiB.
The adapter verifies descriptor size, stream offsets, and digest.

The workload `Wait` RPC has no deadline.
Only service readiness before it is sent uses the control timeout.

## OCI specification

`containerd/spec.rs` builds the base Linux specification.
`container/oci.rs` applies explicit runner policy after that base.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Image["Image config"]
    Task["Task overrides"]
    Engine["Engine defaults"]
    Base["Base OCI spec"]
    Policy["ContainerProcessPolicy"]
    Final["Final OCI spec"]

    Image --> Base
    Task --> Base
    Engine --> Base
    Base --> Final
    Policy --> Final
```

Task command and argument overrides follow Kubernetes container rules.
Task and runner environment values override image values.

The base creates PID, IPC, UTS, and mount namespaces.
`ContainerNetwork::None` also creates a network namespace.
`ContainerNetwork::Host` omits that namespace.

Neither mode provisions an interface, address, route, DNS, NAT, or CNI.
The network mode does not change OCI capabilities.

An empty `ContainerProcessPolicy` preserves the base policy.
Configured values replace only their owned OCI fields.

## Containerd ownership and cleanup

One `ContainerdEngine` receives a random session identifier.
Each native attempt receives a monotonic resource ID within that session.
The snapshot, container, and task share that resource ID.

Snapshot and container records carry labels for:

- manager;
- engine session;
- resource ID;
- model task name;
- generation;
- attempt.

The adapter tracks each remote resource separately.

| State       | Meaning                                             | Cleanup action                            |
|-------------|-----------------------------------------------------|-------------------------------------------|
| `Absent`    | The resource is known not to exist                  | Do not delete                             |
| `Foreign`   | The resource exists but does not match this attempt | Never delete                              |
| `Owned`     | Creation or read-back confirmed this attempt        | Delete after dependent resources are gone |
| `CreateUncertain` | A create may still commit remotely          | Keep charged and read back before cleanup |
| `DeleteUncertain` | A delete needs identity read-back            | Read back before parent cleanup           |

Ownership enters `CreateUncertain` before a mutating create RPC is polled.
Cancellation transfers the in-flight mutation record and ownership state to cleanup.
Cleanup settles that mutation before ownership read-back or deletion.

The adapter verifies resource identity immediately before deletion.
Containerd deletion is keyed only by resource ID. The configured namespace
must not replace that resource between verification and deletion.

Snapshot identity includes its parent and ownership labels.
Container identity includes its snapshotter, snapshot key, and ownership labels.
Task identity requires the confirmed owned container.

Cleanup follows dependency order:

1. terminate and delete the task;
2. delete the container;
3. remove the snapshot;
4. remove local attempt I/O.

Each `ContainerdEngine` owns one bounded cleanup domain.
The default admission limit is 1024 admitted create or attempt lifecycles.
The same limit bounds a separate local I/O domain.
Lifecycle admission is reserved before image resolution. I/O admission is
reserved before attempt-owned local or remote resources are created. Shared
image transfer is charged to lifecycle admission but is not deferred-cleanup
ownership.

```text
active attempt -> explicit cleanup -> release admission
       |
       v
future cancellation -> non-blocking handoff -> isolated cleanup runtime

I/O prepare/remove -> bounded handoff -> dedicated blocking I/O thread
```

The cleanup runtime owns its operating-system thread and containerd channel.
It does not depend on the runtime that polled the attempt.
Filesystem preparation and removal run on the engine-local I/O thread.

Retryable cleanup errors use exponential backoff from 100 milliseconds to 2 seconds.
One cleanup retry window uses 30 seconds by default.
Accepted I/O preparation and a retained mutation settle before that window starts.
Deferred cleanup starts another bounded pass after a retryable failure.
Permanent unresolved ownership is quarantined and remains charged.

`ContainerdEngine::shutdown` closes lifecycle admission and waits for accepted
create lifecycles and attempt ownership.
It then closes the blocking I/O domain even when remote cleanup reports an error.
Both phases share one deadline when the configured duration fits Tokio's
`Instant` range. A larger duration waits without a local deadline.
Deadline expiry returns a retryable error and leaves the worker owned by the engine.
Lost or quarantined ownership returns a permanent error immediately.
Call shutdown after every supervisor that uses the engine has stopped.

Deferred cleanup is process-local. It cannot recover ownership after process
abort, power loss, or `SIGKILL`.

The same cleanup path handles normal completion and failed-create rollback.
Incomplete rollback is reported with both creation and cleanup failures.

Linux attempt I/O uses one private `0700` directory and two `0600` FIFOs.
The configured root must be visible at the same path to the SDK process and containerd.
Writable shared path components require the sticky bit.
FIFO readers stay non-blocking and use the active Tokio I/O driver.
Missing driver support is reported as attempt creation failure.

## Output path

Subprocess and container attempts use the same line pipeline.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Bytes["stdout or stderr bytes"]
    ByteLimit["Byte prefix<br/>drain oversized suffix"]
    Trace["optional tracing copy<br/>lossy UTF-8 and escaped controls"]
    Sink["OutputSink<br/>exact prefix and truncation status"]

    Bytes --> ByteLimit
    ByteLimit --> Trace
    ByteLimit --> Sink
```

Stdout and stderr are independent streams.
Only the opt-in tracing copy uses lossy UTF-8. It preserves tabs and escapes
Unicode control characters, line and paragraph separators, and bidirectional
formatting controls. The output sink receives a borrowed view of the exact bounded bytes;
the composition layer can detach it with one ownership copy. Invalid UTF-8 is
preserved, and truncation is metadata rather than text inserted into the line.

The crate publishes live output only when the build context provides a sink.
It does not retain output.

## Failure boundaries

Each layer owns one error contract.

| Boundary                      | Error                       | Meaning                                       |
|-------------------------------|-----------------------------|-----------------------------------------------|
| Registration and runner setup | `ExecError`                 | Invalid config, router rejection, or host I/O |
| Workload build                | `solti_runner::RunnerError` | Invalid or unsupported workload               |
| Container engine operation    | `ContainerEngineError`      | Retryable or permanent engine failure         |
| Attempt execution             | `taskvisor::TaskError`      | Success, retryable, fatal, or canceled result |

Diagnostic text does not select retry policy.
Typed error classes and operating-system error kinds do.

Cleanup failures are fatal for the current attempt.
They cannot be hidden by cancellation or an earlier execution result.

## Where to make a change

| Change                                  | Start here                                                                                                                                             | Verify here                                 |
|-----------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------|
| Public exports or feature boundary      | [`src/lib.rs`](src/lib.rs), [`Cargo.toml`](Cargo.toml)                                                                                                 | feature checks and rustdoc                  |
| Shared isolation value                  | [`src/isolation/`](src/isolation)                                                                                                                      | isolation, host, and container policy tests |
| Host policy preparation                 | [`src/host/policy.rs`](src/host/policy.rs)                                                                                                             | host policy tests                           |
| Process state or rlimits                | [`src/host/process.rs`](src/host/process.rs), [`src/host/limits.rs`](src/host/limits.rs)                                                               | process and limit tests                     |
| Cgroup ownership or cleanup             | [`src/host/cgroups.rs`](src/host/cgroups.rs)                                                                                                           | cgroup and process-domain tests             |
| Host identity, capabilities, or seccomp | [`src/host/security.rs`](src/host/security.rs), [`src/host/capability.rs`](src/host/capability.rs)                                                     | security and capability tests               |
| Subprocess settings, env, or cwd        | [`src/subprocess/backend.rs`](src/subprocess/backend.rs), [`src/subprocess/boundary.rs`](src/subprocess/boundary.rs)                                   | backend and boundary tests                  |
| Command or script materialization       | [`src/subprocess/runner.rs`](src/subprocess/runner.rs), [`src/subprocess/script.rs`](src/subprocess/script.rs)                                         | runner and script tests                     |
| Subprocess termination or drop          | [`src/subprocess/domain.rs`](src/subprocess/domain.rs), [`src/subprocess/runner.rs`](src/subprocess/runner.rs)                                         | domain and cancellation tests               |
| Generic container lifecycle             | [`src/container/engine.rs`](src/container/engine.rs), [`src/container/runner.rs`](src/container/runner.rs)                                             | container runner tests                      |
| Container process policy                | [`src/container/policy.rs`](src/container/policy.rs), [`src/container/oci.rs`](src/container/oci.rs)                                                   | policy and OCI tests                        |
| Containerd config or probe              | [`src/container/containerd/config.rs`](src/container/containerd/config.rs), [`src/container/containerd/engine.rs`](src/container/containerd/engine.rs) | config and probe tests                      |
| Image resolution or metadata            | [`src/container/containerd/image.rs`](src/container/containerd/image.rs)                                                                               | image tests                                 |
| Containerd ownership or lifecycle       | [`src/container/containerd/engine.rs`](src/container/containerd/engine.rs), [`src/container/containerd/cleanup.rs`](src/container/containerd/cleanup.rs), [`src/container/containerd/rpc.rs`](src/container/containerd/rpc.rs) | engine ownership and cleanup tests          |
| OCI base specification                  | [`src/container/containerd/spec.rs`](src/container/containerd/spec.rs)                                                                                 | spec tests                                  |
| Containerd output pipes                 | [`src/container/containerd/io.rs`](src/container/containerd/io.rs)                                                                                     | Linux I/O tests                             |
| Shared output behavior                  | [`src/output.rs`](src/output.rs)                                                                                                                       | output tests                                |
| Runner registration                     | [`src/registration.rs`](src/registration.rs), [`src/error.rs`](src/error.rs)                                                                           | runner registration tests                   |
| User-facing usage                       | [`README.md`](README.md), [`src/lib.rs`](src/lib.rs)                                                                                                   | README and rustdoc doctests                 |
