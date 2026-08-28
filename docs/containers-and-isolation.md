---
title: Containers and isolation
description: Connect container engines, retain cleanup ownership, and choose explicit host and OCI process controls.
---

# Containers and isolation

The SDK separates a workload from the policy and engine that execute it.
A Container workload carries image, command, arguments, and environment.
The application chooses the engine, endpoint, resource limits, security controls, and cleanup owner.
A subprocess, an OCI container, and an in-process runner have different isolation boundaries.

## Participants and features

| Component | Role and reason to use it |
|---|---|
| `solti-model` | Defines `ContainerSpec` and the outer Task policy without choosing a daemon. |
| `solti-runner` | Routes `solti.io/v1/Container` to a registered backend and carries build context. |
| `solti-exec/container` | Supplies `ContainerRunner`, engine interfaces, ownership bindings, output handling, and `ContainerProcessPolicy`. It does not select an engine. |
| `solti-exec/containerd` | Adds the native containerd 2.x adapter and enables `container`. Actual attempts require Linux. |
| `solti-exec/host-process` | Supplies host-process controls for subprocesses and custom process backends. It is not the OCI policy renderer. |
| `solti-core` and Taskvisor | Own desired state, runtime admission, attempt timeout/restart, cancellation, and observable lifecycle. They do not own an external container daemon. |

`solti-exec` has no default features.
The facade uses `exec-container` for engine-neutral integration and `exec-containerd` for the native adapter.
Host seccomp uses the `seccomp` feature, which enables `host-process`.
For the subprocess backend, select both `subprocess` and `seccomp`; a custom host-process backend does not need `subprocess`.
Native OCI seccomp is rendered by `containerd` without that host-process feature.
`WasmSpec` is model-only. No bundled WASM engine or in-process WASM isolation is enabled by these features.

## Build once, create resources per attempt

```text
Container Task → router → validate and build reusable TaskRef

each attempt
    → engine.create_attempt: resolve image and create stopped resources
    → take output streams, with exit observation already armed
    → start → wait or cancel
    → terminate when needed → drain output → cleanup
```

`ContainerRunner::build_task` performs no engine I/O and does not call `probe`.
It validates process inputs and stores the image request, merged environment, and runner policy.
The application chooses when to probe; native `ContainerdEngine::connect` performs its startup checks explicitly.
Core's build deadline does not include image transfer because that work begins inside an execution attempt.

Every attempt gets a distinct request identifier.
Retrying the outer Task creates a fresh attempt; it does not reuse a previously running container.
The runner owns at most two output-reader tasks.
Engine lifecycle methods run inline inside the attempt future, not as runner-detached background work.

## Choose a custom engine ownership contract

Implement `ContainerEngine::probe` and `create_attempt`.
The latter must return a stopped `ContainerAttempt` with exit observation armed before `start`.
An attempt supplies optional stdout/stderr readers and implements `start`, `wait`, `terminate`, and `cleanup`.
Termination and cleanup must be idempotent and act only on resources the attempt owns.
Retries must retain completed cleanup steps rather than repeating completed destructive actions.

Register a custom engine through an explicit `ContainerEngineBinding`:

| Binding | Provider's obligation |
|---|---|
| `drop_releases` | Dropping an in-flight create future or attempt synchronously releases all accepted resources and forward work. Nothing remains owned after `Drop`. |
| `pre_admitted_finalizer` | Reserve finite cleanup capacity before the first resource acquisition or remote mutation. Drop transfers confirmed and uncertain ownership without unbounded work. The provider defines failure behavior and awaitable shutdown. |

These constructors record the provider's declaration; they cannot verify its truth.
For a custom finalizer engine, implement `ContainerEngineFinalizer` and retain the handle returned by `pre_admitted_finalizer_with_shutdown`:

```rust,no_run
use std::sync::Arc;
use solti_exec::container::{
    ContainerEngineBinding, ContainerEngineFinalizer, ContainerEngineShutdownHandle,
    register_container_runner,
};
use solti_runner::RunnerRouter;

fn register<E: ContainerEngineFinalizer>(
    router: &mut RunnerRouter,
    engine: Arc<E>,
) -> Result<ContainerEngineShutdownHandle, solti_exec::ExecError> {
    let (binding, shutdown) =
        ContainerEngineBinding::pre_admitted_finalizer_with_shutdown(engine);
    register_container_runner(router, "custom", binding)?;
    Ok(shutdown)
}
```

Call the retained shutdown handle after every supervisor and other task user of this engine has stopped.
A concrete `Arc<ContainerdEngine>` automatically selects the native pre-admitted-finalizer binding.

The [custom engine example](../crates/solti-exec/examples/custom_container_engine.rs) is an in-memory teaching implementation with `drop_releases` ownership.
It demonstrates the interface and event order, not real container execution or enforcement of a process policy.

## Handle cancellation and engine errors

Preexisting cancellation prevents engine creation.
Once `create_attempt` is running, cooperative cancellation waits for it to return; the runner cannot terminate a resource for which it has no attempt value yet.
If a created attempt arrives after cancellation, the runner cleans it without starting it.
An error returned by create remains a create failure.

While a container runs, cancellation selects termination, wait, output drain, and cleanup.
An outer timeout or physical force-drop can interrupt an inline engine method.
Cleanup after that drop belongs to the declared engine ownership contract, not to an implicit runner finalizer.
Synchronous code that does not yield can delay physical drop until it returns control to Tokio.

Retryable create, start, and normal wait errors become retryable task failures; permanent ones become fatal errors.
A nonzero container exit is retryable and retains the exit code.
There is no subprocess-style `failOnNonZero` switch for Container workloads.
Termination, wait-after-termination, and cleanup errors are fatal, including when cancellation was requested.
Output-reader failures are diagnostic and do not replace the container result.
Taskvisor applies the outer restart policy to this final result.

## Connect the native containerd adapter

The application supplies an explicit Unix socket, namespace, snapshotter, and OCI runtime.
The adapter does not discover or start a daemon, use CRI, or configure CNI.
Startup validates containerd major version 2 and the configured snapshotter, image platform, and runtime compatibility.

This setup fragment belongs inside an async, fallible application function on a prepared Linux host:

```rust,no_run
# #[cfg(target_os = "linux")]
# async fn configure_containerd() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use solti_exec::container::containerd::{
    ContainerNetwork, ContainerdConfig, ContainerdEngine,
};
use solti_exec::container::{
    ContainerProcessPolicy, ContainerRunnerConfig, register_container_runner_with_config,
};
use solti_exec::isolation::SeccompPolicy;
use solti_runner::RunnerRouter;

let config = ContainerdConfig::new(
    "/run/containerd/containerd.sock",
    "solti",
    "overlayfs",
    "io.containerd.runc.v2",
)
.with_network(ContainerNetwork::None)
.with_io_root("/run/solti/containerd-io");
let engine = Arc::new(ContainerdEngine::connect(config).await?);
let policy = ContainerProcessPolicy::new()
    .with_capabilities([])
    .with_no_new_privileges(true)
    .with_umask(0o077)
    .with_seccomp(SeccompPolicy::DenyHostControl);
let mut router = RunnerRouter::new();
register_container_runner_with_config(
    &mut router,
    "containerd",
    engine.clone(),
    ContainerRunnerConfig::new().with_process_policy(policy),
)?;
# let _ = (router, engine);
# Ok(())
# }
```

The paths and plugin names are explicit example settings, not discovered host facts.
Create the intended host configuration and permissions before connecting.
Retain `engine` to call `engine.shutdown().await` after its users stop.

### Image and command boundaries

Native image resolution invokes pull-and-unpack on every attempt.
An already present image does not make this a documented cache-only or registry-free path.
Registry host configuration is optional and interpreted by the daemon.
Shared image content is not deleted with attempt resources.

A nonempty workload `command` replaces the image entrypoint.
Nonempty workload `args` replace image Cmd; empty arguments preserve image Cmd.
No shell is inserted implicitly.
Environment precedence is image, then task, then runner.
The working directory comes from the image and must be absolute; an empty image value becomes `/`.

Without an explicit credentials policy, image `User` must be empty or an exact numeric `UID:GID` pair accepted by the adapter.
Each numeric ID must be at most `i32::MAX`.
Named users and a numeric UID without a GID are rejected; the adapter does not resolve `/etc/passwd` inside the image.
Explicit numeric credentials bypass image-user resolution.
The adapter does not create a user namespace; these IDs are in the host user namespace.

### Network and local I/O boundaries

`ContainerNetwork::None`, the default, creates an empty network namespace.
It does not add interfaces, addresses, routes, DNS, NAT, bridge configuration, or CNI.
`Host` omits that namespace and shares the host network namespace.
The adapter does not mount the host's `/etc/hosts` or `/etc/resolv.conf` into the container.
Network selection does not remove capabilities; configure the process policy separately.

The native base specification includes PID, IPC, UTS, and mount namespaces, a default capability set, and `noNewPrivileges`.
An empty runner policy preserves those engine defaults; it does not mean that all capabilities are dropped.

The SDK process and daemon must access the I/O root at the same absolute path.
Each attempt uses a private mode-0700 directory and mode-0600 FIFOs.
The Linux path check rejects symlink components and untrusted owners; group- or world-writable ancestors require the sticky bit.
The caller's Tokio runtime needs I/O enabled for FIFO readers.
Mount-path visibility, daemon permissions, and OCI runtime support are deployment responsibilities.

### Native cleanup and deadlines

The default operation budgets are 30 seconds for control calls, 10 minutes for image transfer, and 30 seconds for cleanup.
Configure them with `ContainerdConfig`; they are separate from the outer Task timeout.
The control deadline does not bound workload wait.
Cleanup timing accounts for accepted local I/O and retained mutation work before its cleanup retry window.
Deferred cleanup can start another window after a retryable failure.
It is not a universal 30-second end-to-end resource-release guarantee.

The default capacity of 1024 bounds admitted create lifecycles, active attempts, and deferred cleanup ownership together.
The same number independently bounds local I/O ownership.
Lifecycle admission is reserved before image resolution and fails before that work when full or closed.
Local I/O and deferred remote cleanup have engine-owned workers.

The native owner tracks confirmed and uncertain creation/deletion outcomes.
Cleanup verifies ownership, then removes the owned task, container, snapshot, and local I/O in order.
Creation rollback uses the same retained ownership path.
Foreign resources are not adopted or deleted.
Other namespace users must not replace an owned ID between verification and deletion; the daemon's ID-based operation is not an atomic ownership comparison.

Cleanup retries do not rerun the workload.
Unresolved permanent ownership is quarantined and remains charged to capacity.
This ownership is process-local; it is not recovery after agent `SIGKILL`, power loss, or machine loss.

`engine.shutdown().await` closes lifecycle admission and waits for accepted ownership.
It is terminal, idempotent, and safe to retry after cancellation.
Local I/O shutdown is still attempted when remote cleanup reports an error.
A timeout or quarantined/lost ownership is an error, not proof that all resources were removed.
See [cancellation and shutdown](cancellation-and-shutdown.md) and [production boundaries](production-boundaries.md).

## Choose host and OCI policy deliberately

These controls are runner configuration. They are not authority granted by a Task payload.
Unsupported requested host controls fail validation or prevent process start.
An engine provider is responsible for enforcing the container policy it receives.

| Control | Host-process path | Native OCI path |
|---|---|---|
| Process state | Unix `ProcessConfig`: signal reset, session, and umask. The subprocess runner also owns its dedicated Unix session/group. | OCI process umask through `ContainerProcessPolicy`; native namespaces come from its base specification. |
| POSIX limits | Unix `RlimitConfig`: NOFILE, FSIZE, and CORE. Prepared values are clamped to inherited hard limits and applied to soft and hard limits. | Configured limits replace the matching OCI entries; host-side clamping is not applied. |
| CPU, memory, PIDs | Linux cgroup v2 through `CgroupLimits` and an owned per-attempt child cgroup. | OCI Linux resource fields applied by the runtime. |
| Credentials | Linux real, effective, and saved UID/GID plus the exact supplementary-group list. | Numeric OCI UID/GID and supplementary groups, without a user namespace. |
| Capabilities | Linux `SecurityConfig`: `drop_all_caps` with an optional `keep_caps` allowlist. Retained capabilities must be available to the parent. | A configured capability list replaces all five OCI sets. `None` preserves the base; an empty list drops all. |
| Namespaces | Linux mount, network, IPC, UTS, and cgroup flags. No PID or user namespace in `Namespaces`. A new mount namespace uses private propagation. | Native base PID, IPC, UTS, and mount namespaces; network selected separately. No user namespace. |
| Privilege gain | `no_new_privs` before exec. Credentials and capability dropping require it explicitly. | `with_no_new_privileges(true)`. Credentials and capability replacement require it explicitly. |
| Syscalls | Linux host `seccomp` feature installs `DenyHostControl` before exec. | The native adapter renders that policy into OCI seccomp for the runtime. |

`DenyHostControl` is a fixed denylist returning `EPERM` for selected host-control syscalls.
Other syscalls remain allowed; this is not a general syscall allowlist sandbox.
The host x86_64 LP64 path rejects x32 calls. Native OCI alternate-ABI behavior depends on the supported runtime/libseccomp path; do not assume a custom runtime has identical behavior.
Its implicit no-new-privileges setting does not satisfy the explicit credentials/capability configuration requirement.
In an OCI policy, `false` no-new-privileges and `Disabled` seccomp do not clear protections already present in the base specification.

### Own a host cgroup through cleanup

`HostProcessPolicy::prepare` validates reusable settings and prepares the configured cgroup parent.
When limits are configured, each `prepare_attempt` creates an owned child cgroup and sets `cgroup.max.depth=0` before the process starts.
An empty policy creates no cgroup.

The application must provide a writable cgroup v2 delegation and usable controllers.
The SDK does not establish system-wide delegation.
An explicit parent must already exist; otherwise the current process's cgroup is used as the parent for attempt cgroups.
Do not let workloads move themselves through a writable parent `cgroup.procs` or replace owned cgroup paths.

`cgroup.kill`, when available, reaches descendants that remain in the owned cgroup even if they leave the original process group.
Without it, subprocess termination falls back to its group boundary and cannot reach descendants that escape that group.
Cleanup requires an empty cgroup and matching path identity before removal.

For a custom process backend, consume `PreparedHostProcessAttempt` with `apply_to_command`, retain the returned `AttemptProcessDomain`, terminate your own process boundary, handle cgroup termination, wait/reap, then clean the domain.
The domain is not a process supervisor and does not own your child handle.
The [host policy example](../crates/solti-exec/examples/host_process_policy.rs) demonstrates this low-level sequence without cgroups.

### Preserve termination authority

On Linux, subprocess credentials that change the child UID away from both parent real and effective UIDs require parent effective `CAP_KILL`.
Keeping child `CAP_SETUID` requires that parent authority as well.
The subprocess runner checks at construction and before each attempt; capabilities are thread-scoped, and the application must preserve the required authority while the runner is active.
Child capability configuration does not grant capabilities to the agent or its finalizer.

If credentials and capabilities remain inherited, the workload must preserve an identity the parent can signal or the parent must retain the necessary authority.
For fixed identities, configure explicit credentials, no-new-privileges, and capability removal without `CAP_SETUID`/`CAP_SETGID`.
A retained `CAP_SYS_RESOURCE` can also allow the child to raise hard rlimits later.
These controls do not turn unrestricted host filesystem access into a sandbox.

## Run existing examples

```bash
cargo run -p solti-exec --example custom_container_engine --features container
cargo run -p solti-exec --example containerd_config --features containerd
cargo run -p solti-exec --example containerd_config --features containerd -- --connect
cargo run -p solti-exec --example container --features containerd
cargo run -p solti --example task_containerd --features core,exec-containerd
```

`custom_container_engine` runs only the in-memory engine.
`containerd_config` prints explicit settings without contacting a daemon unless `--connect` is passed.
`container` exercises one direct native attempt; `task_containerd` includes core reconciliation and Taskvisor supervision.
Actual container examples skip execution on non-Linux hosts.

The actual-container examples read `SOLTI_CONTAINERD_SOCKET`, `SOLTI_CONTAINERD_NAMESPACE`, `SOLTI_CONTAINERD_SNAPSHOTTER`, `SOLTI_CONTAINERD_RUNTIME`, `SOLTI_CONTAINER_IMAGE`, and `SOLTI_CONTAINER_NETWORK` (`none` or `host`).
Their default image is `docker.io/library/alpine:3.21`; their commands require `/bin/sh` and the shell operations shown in each source.
A replacement image must satisfy those commands and the image-user rules above.
These examples use the configuration's default temporary I/O root; deployments needing a different shared path must configure `with_io_root` in their application.
They do not install containerd, start a daemon, or provision host networking.

## See also

- [Subprocesses](subprocesses.md): command/script transport, descriptor boundaries, and finalizer ownership.
- [Routing and custom runners](routing-and-custom-runners.md): registration, build budgets, and extension contracts.
- [Build an agent](building-an-agent.md): retain service and backend shutdown handles.
- [Example catalog](example-catalog.md): runnable paths and prerequisites.
- Source: [engine contracts](../crates/solti-exec/src/container/engine.rs), [container lifecycle](../crates/solti-exec/src/container/runner.rs), [container policy](../crates/solti-exec/src/container/policy.rs), [native configuration](../crates/solti-exec/src/container/containerd/config.rs), [OCI construction](../crates/solti-exec/src/container/containerd/spec.rs), [host policy](../crates/solti-exec/src/host/policy.rs), and [Linux security](../crates/solti-exec/src/host/security.rs).
