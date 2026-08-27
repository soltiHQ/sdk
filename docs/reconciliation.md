---
title: Reconciliation
description: Follow a desired generation through latest-wins coordination, runner construction, and runtime intake.
---

# Reconciliation

Reconciliation turns one committed desired generation into a Taskvisor submission. Core owns this process after the resource write. The write caller does not wait for it to finish.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Supplies the desired spec and the generation-scoped `Reconciled` condition. |
| `solti-core` | Coordinates work by task name, manages preflight deadlines, binds runtime identity, and projects acceptance or failure. |
| `solti-runner` | Selects a registered runner and constructs a `TaskRef` under the managed build scope. |
| Concrete runner | Validates backend-specific work and owns its build future. |
| Taskvisor | Accepts controller commands, enforces slot/runtime admission, and reports task outcomes. |

## The handoff sequence

```text
committed manifest
    -> latest desired request for the task name
    -> routed build, or caller-supplied Embedded TaskRef
    -> prepare Taskvisor submission
    -> settle the previous runtime binding
    -> bind resource UID + generation to Taskvisor ID
    -> await Taskvisor command intake
    -> observe attempts and the direct final outcome
```

The resource initially has `Reconciled=Unknown`. Successful Taskvisor command intake allows `Reconciled=True`; it does not require execution to start. The reconciliation path installs direct completion tracking before its explicit acceptance update. Runtime observations can also advance status once accepted work starts. The controller can still queue or reject a command after intake.

## Latest-wins coordination

Each task name has one active reconciliation request and at most one pending request. Scheduling newer desired state cancels the active request's preflight signal and replaces the pending request with the latest one.

This bounds pending reconciliation per name. It also means that an intermediate desired generation may never reach a runner or Taskvisor. The newest committed resource remains the desired state even when an older request is still disposing its build or cleaning up runtime ownership.

The coordinator rechecks resource UID, generation, cancellation, and shutdown state at handoff boundaries. Old work must not publish acceptance or failure over a newer resource generation.

This is not a staged rollout or an availability guarantee. It does not keep two generations ready and switch traffic between them.

## Routed construction

The router selects by exact workload group/version and kind, then applies `runnerSelector` to registered runner labels. The first matching registration wins. No match is a routing failure. Embedded resources bypass routing and use the supplied task object.

For a routed resource, core performs the build before cancelling a previous accepted runtime. If selection or construction fails at this stage, the new generation records `Reconciled=False`; the build-failure branch has not yet requested removal of that previous runtime. The stored desired state is still the new generation.

`ReconciliationConfig` controls managed build work:

| Setting | Default | Boundary |
| --- | --- | --- |
| `build_timeout` | 30 seconds | One routed preflight, starting before root admission and including nested construction and admission waits. |
| `max_concurrent_builds` | 32 | Concurrent outer build scopes. |
| `max_concurrent_builds_per_runner` | 8 | Builds using a registered runner, including nested scoped builds. |

Capacity pressure waits for build admission. It is separate from a Taskvisor slot rejection or runtime attempt-permit wait. The build deadline does not bound the complete create/apply request, previous-runtime cleanup, Taskvisor intake, or task execution.

Nested composition uses `RunnerCatalog::build_scoped_with_cancellation`. It reuses the root global permit and acquires the selected runner's permit. Recursive build paths and detected admission cycles have typed router errors. A direct `RunnerRouter::build` or unmanaged catalog build outside core does not acquire core's managed limits.

`BuildCancellation` belongs to construction. It is not the `TaskContext` cancellation signal used by an execution attempt. Supersession, user cancellation, shutdown, and the build deadline can stop preflight. Core aborts the asynchronous build task when needed; it cannot preempt synchronous code that does not return from a poll or destructor.

See [routing and custom runners](routing-and-custom-runners.md) and [chains](chains.md) for the construction contract inside runners.

## Replace the runtime binding

After successful construction, core maps SDK timeout, restart, backoff, retry limit, and admission policy into a Taskvisor submission. The SDK slot becomes the controller slot key; the generated runtime name is separate from the resource name.

The per-name runtime lock serializes replacement with cleanup. Core requests cancellation of a previous binding and waits for its logical cleanup and SDK observer settlement before binding the new submission. Cleanup failure records a reconciliation failure instead of silently advancing to the replacement.

The new resource-to-runtime binding is installed before Taskvisor can publish its events. Before intake succeeds, status remains pending and the condition can explain that Taskvisor intake is pending.

Previous logical cleanup does not prove physical task exit or that the controller has already changed the slot to idle. The new submission still follows its configured `DropIfRunning`, `Queue`, or `Replace` policy. See [lifecycle and admission](lifecycle-and-admission.md).

## Acceptance, rejection, and failure

Keep these outcomes separate:

| Boundary | Observation |
| --- | --- |
| Manifest validation or retained-state admission fails | The write returns a `CoreError`; no successful desired-state commit is reported. |
| Routing, construction, preparation, or previous-runtime cleanup fails | The committed resource records `Reconciled=False` for the affected generation. |
| Taskvisor command intake succeeds | `Reconciled=True`; execution need not have started. |
| Controller later rejects accepted work | A typed rejection becomes an execution-phase result, possibly with attempt zero. |
| An attempt runs and fails | Attempt state and retained history change; restart policy may schedule another attempt. |
| The managed runtime finishes | Core consumes the direct Taskvisor outcome and settles its resource binding. |

Diagnostic condition reasons distinguish cases such as runner build timeout, build panic, and preparation failure. Treat them as diagnostics alongside the typed status, not as a substitute for resource identity or execution state.

Core observes attempt events for history and intermediate status. Those events are bounded and best-effort. Each accepted submission also has direct completion tracking, which provides the final runtime result without depending on delivery of every event. An event-delivery gap can leave incomplete attempt history even when final task state is settled.

## Retrying reconciliation

There is no unconditional rebuild on every apply. An identical manifest retries only a retained `Reconciled=False` resource. The retry uses the same desired generation. A metadata-only write does not enter this retry branch.

Changing the spec schedules another generation. Changing only the executable pointer supplied to an Embedded apply is not a spec change; use a changed Embedded revision when the implementation changes.

User cancellation can stop pre-intake work and record reconciliation failure without any `TaskRun`. If shutdown stops preflight first, an existing pending condition can remain `Unknown`. Shutdown is not a promise that every unaccepted desired generation receives a synthetic execution result.

## Examples and source

- [Embedded lifecycle example](../crates/solti-core/examples/embedded_lifecycle.rs): latest desired generation replacing a running Embedded task.
- [Routed output example](../crates/solti-core/examples/routed_output.rs): desired-state commit, runner construction, intake, and execution.
- [Coordinator and handoffs](../crates/solti-core/src/runtime/reconciler.rs), [runtime observer](../crates/solti-core/src/runtime/observer.rs), [runner selection](../crates/solti-runner/src/router.rs), and [build admission](../crates/solti-runner/src/admission.rs).

## See also

- [Managing tasks](managing-tasks.md) for apply and guard behavior.
- [Lifecycle and admission](lifecycle-and-admission.md) for the runtime half of the process.
- [Cancellation and shutdown](cancellation-and-shutdown.md) for owned cleanup.
