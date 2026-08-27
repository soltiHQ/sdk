---
title: Cancellation and shutdown
description: Understand owned cancellation, deletion, logical completion, and the shared supervisor shutdown drain.
---

# Cancellation and shutdown

Cancellation stops current work while retaining its desired resource. Deletion also removes retained state. Shutdown closes the supervisor's operation admission and drains SDK-owned work. These operations do not have the same completion boundary.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Carries identity guards, reconciliation conditions, phases, and attempt records. |
| `solti-core` | Serializes operations by resource name, cancels preflight, owns cleanup workers, and coordinates shutdown. |
| `solti-runner` | Exposes cooperative cancellation for builds before runtime acceptance. |
| Taskvisor | Cancels accepted queued or registered tasks and commits logical terminal outcomes. |
| Application task and sinks | Must return from work and callbacks for the corresponding physical or callback drain to finish. |

## Cancel one resource

`cancel_task(&name).await` retains desired state and run history.

1. Core serializes the operation with create, apply, delete, and other cancellations for that name.
2. It signals scheduled reconciliation before waiting for the runtime-operation lock.
3. It waits for that reconciliation's owned cleanup to settle.
4. If a runtime binding exists, it asks Taskvisor to cancel that exact runtime ID.
5. It waits for the SDK observer to settle the confirmed logical outcome.

Before Taskvisor intake, cancellation drops prepared work and records `Reconciled=False` without creating a `TaskRun`. If shutdown's preflight stop wins that branch first, the existing pending `Reconciled=Unknown` condition can remain.

After intake, Taskvisor handles queued or running work. A queued task can end without any attempt starting. A running task receives its cancellation signal and can reach a logical force-abort if it does not stop within the runtime grace window.

An unknown name returns `CoreError::NotFound`. A later apply can reconcile the resource again; cancellation does not install a permanent disabled flag. An unchanged apply retries only `Reconciled=False`, not every terminal runtime phase.

## Delete one resource

`delete_task(&name).await` cancels scheduled reconciliation, requests logical cleanup of the current bound runtime, then removes the Task and its retained run history and output-channel lookup.

An unguarded delete of a missing resource returns `Ok(())` before SDK-owned delete registration or persistence admission. Guarded delete and `_where` delete instead report missing or hidden resources as `NotFound`.

A Taskvisor cancellation failure prevents normal completion of the delete path. The API reports that failure rather than reporting a successful removal. State mutation admission can also close during shutdown.

Use `WritePreconditions` to protect cancellation or deletion from stale state. A UID guard distinguishes a resource from a same-name replacement. A resource-version guard also detects metadata or status changes. Guard mismatch is `CoreError::Conflict`; the checked operation is not performed.

The visibility predicate in `_where` methods runs under the per-name lock. It must be pure, non-blocking, and must not call back into `SupervisorApi`.

## Who owns an accepted operation

Cancel and delete transfer ownership to an SDK worker once that worker is registered. Dropping the API future afterward stops only the caller's wait. Shutdown drains registered operations.

This ownership transfer matters when a request handler is cancelled or its response deadline expires. A lost response is not proof that the underlying operation was cancelled. Read retained state or join shutdown as appropriate for the surrounding process.

## Logical completion is not physical exit

| Boundary | What has happened |
| --- | --- |
| Reconciliation stopped | No more work from that cancelled preflight is being handed off by its owned coordinator. |
| Taskvisor logical outcome | The runtime has committed a final outcome and performed its logical registry cleanup. |
| SDK observer settlement | Resource projection and exact runtime-binding cleanup have settled for that outcome. |
| Physical attempt exit | Task code has actually returned or its future has been destroyed after the runtime can regain control. |
| Controller slot idle | The controller has processed physical completion and released slot ownership. |

`ForceAborted` is a logical result. Non-cooperative code can remain physically active afterward. A successful SDK cancel or delete does not prove that such code stopped accessing external resources.

Even after ordinary completion, the controller's slot transition can follow SDK settlement. Do not start a same-slot `DropIfRunning` submission on the assumption that cancel, delete, or a terminal phase is an idle barrier. [Lifecycle and admission](lifecycle-and-admission.md) explains the policies for that handoff.

## Deadline scopes

| Setting or operation | Scope |
| --- | --- |
| `ReconciliationConfig::build_timeout` | Routed construction, including build-admission waits. |
| `TaskSpec.timeout` | One execution attempt after `Task::spawn` returns its future. |
| Taskvisor `SupervisorConfig::grace` | Cooperative stop window before logical force-abort. |
| Taskvisor `subscriber_shutdown_timeout` | Shared deadline for draining Taskvisor subscriber queues. It can drop queued events but cannot interrupt a callback already running. |
| Core's bound-runtime cancellation wait | Taskvisor cancellation uses the configured grace plus one second for its terminal wait. This is not an end-to-end SDK request deadline. |
| `shutdown_with_timeout(duration)` | This caller's wait for the shared SDK shutdown operation. |

Taskvisor's terminal cancellation timer does not include all controller or registry command-admission waits before the cancellation is claimed. Core also waits for per-name coordination, preflight settlement, observer work, and any required persistence admission.

No Tokio deadline can forcibly interrupt a blocking synchronous poll or destructor. A caller timeout does not prove that physical task code or external side effects stopped.

## Shut down the supervisor

`shutdown().await` starts or joins one shared SDK-owned shutdown operation. Later calls observe the same cached outcome.

The order is:

1. Close operation admission, close Task watches, and signal retention and preflight workers to stop.
2. Drain already registered delete operations.
3. Shut down Taskvisor, then wait for tracked cancellation, reconciliation, and completion work. Finalize safe pending cleanup after confirmed runtime shutdown.
4. Drain state-persistence and output-persistence workers.
5. Publish the shared shutdown outcome.

Watch closure occurs before runtime completion. An output stream may remain open while a runner retains its sink sender. Neither stream ending nor a final output marker replaces waiting for `shutdown`.

`shutdown_with_timeout(duration).await` uses the same operation and ordering. On deadline expiry it returns `CoreError::ShutdownTimedOut`, but the owned coordinator continues draining. It does not forcibly stop a task or callback. A later caller can wait for the same operation again.

Shutdown reports Taskvisor failures as `CoreError::Supervisor` and an unexpectedly stopped SDK shutdown or persistence worker as `ShutdownCoordinatorStopped`.

## Persistence callbacks and shutdown

State and output persistence callbacks, including sink destructors, must eventually return. Plain shutdown can wait indefinitely for a blocking callback; a caller deadline only bounds that caller's wait.

Polling `SupervisorApi::shutdown` or `shutdown_with_timeout` from inside a persistence `on_event` callback panics before shutdown starts. Sink destruction is outside that callback guard; do not await shutdown from a sink destructor either. Waiting for another thread that calls shutdown can deadlock the drain and is also forbidden. A state sink must not mutate `TaskState`, directly or through another thread it waits for.

Dropping persistence owners detaches their worker handles instead of blocking a destructor indefinitely. Explicit supervisor shutdown is the observable accepted-event drain boundary. See [persistence](persistence.md).

## Examples and source

- [Embedded lifecycle example](../crates/solti-core/examples/embedded_lifecycle.rs): observe cooperative cancellation during generation replacement and explicitly shut down.
- [Public cancel, delete, and shutdown operations](../crates/solti-core/src/supervisor/mod.rs).
- [Preflight cancellation](../crates/solti-runner/src/cancellation.rs), [observer settlement](../crates/solti-core/src/runtime/observer.rs), and [persistence drain](../crates/solti-core/src/persistence.rs).

## See also

- [Managing tasks](managing-tasks.md) for operation guards and return values.
- [Reconciliation](reconciliation.md) for superseding pending work.
- [Output and history](output-and-history.md) for retained records and stream lifetime.
