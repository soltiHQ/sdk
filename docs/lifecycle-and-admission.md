---
title: Lifecycle and admission
description: Distinguish desired-state admission, build capacity, slot policy, attempts, and final runtime outcomes.
---

# Lifecycle and admission

A resource passes several independent admission boundaries before an attempt runs. Each boundary has its own capacity, wait behavior, and failure surface.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Describes slot, restart, backoff, timeout, retry limit, and admission policy. |
| `solti-core` | Admits retained desired state, coordinates builds, maps policies, and projects execution state. |
| `solti-runner` | Applies managed construction limits before an executable task exists. |
| Taskvisor | Owns controller intake, keyed slot admission, registry membership, attempt permits, retries, and cleanup. |

## Admission is a sequence

| Boundary | Configuration | What pressure means |
| --- | --- | --- |
| Retained resource | Core `StateConfig` | A new name or positive manifest growth can be rejected by a retained-state limit. Another task is not evicted to make room. |
| State-event persistence | Core `PersistenceConfig` | With a state sink installed, mutation waits for bounded event and byte admission before commit. |
| Runner construction | Core `ReconciliationConfig` | Routed builds wait for global and per-runner permits within the build deadline. |
| Taskvisor ownership and controller intake | Taskvisor `SupervisorConfig` and `ControllerConfig` | Async submission can wait before command intake succeeds. |
| Slot and registry admission | Taskvisor controller and registry limits | Accepted commands can be rejected later, including queue, slot-count, pending, or registry limits. |
| Attempt concurrency | Taskvisor `SupervisorConfig::max_concurrent` | A registered task waits for an attempt permit before starting execution. |

Configure these through `SupervisorApiBuilder::with_state_config`, `with_persistence_config`, `with_reconciliation_config`, `with_runtime_config`, and `with_controller_config`.

Do not use retained-task count as a proxy for running attempts. Retained terminal resources still count toward state admission. A runtime can be registered while waiting for a permit, and its permit is not held during retry backoff.

Taskvisor's ownership budget also includes subscribers and user task values retained through queueing, physical execution, and isolated destruction. Core always installs its own observer subscriber; external subscribers consume additional ownership capacity. Logical completion does not necessarily release every ownership charge.

## Slot policy

`TaskSpec.slot` is a concurrency key, not a resource name. Different resources can target the same slot.

| `AdmissionPolicy` | Busy-slot behavior |
| --- | --- |
| `DropIfRunning` | Reject the new submission while the slot is not idle. This is the model default. |
| `Queue` | Append to the bounded slot FIFO. Start only after earlier ownership is released. |
| `Replace` | Request removal of the current owner and place the replacement next. It does not start a second owner immediately. |

`Queue` is not an unbounded waiting promise. `ControllerConfig::max_slot_queue` bounds pending work behind an owner. A zero depth rejects queued work behind a busy owner. Controller total-pending and slot-count limits are separate.

`Replace` does not use the per-slot Queue depth check, but remains subject to the other controller limits. A newer replacement can supersede an earlier pending replacement.

Controller slot reuse follows physical completion of the prior execution path. A logical `ForceAborted` result can therefore leave the slot occupied while non-cooperative code is still active. Even ordinary logical completion and SDK observer settlement can precede the controller's later idle transition.

Neither a terminal SDK phase nor `cancel_task().await` is an idle-slot barrier. A following `DropIfRunning` submission to the same slot can still be rejected. Choose the policy whose contract matches the process; do not infer idle state from resource status.

## Attempts and restart policy

Taskvisor owns one managed runtime task and asks its `Task` implementation for a fresh future on each attempt. SDK generation identifies the desired spec; attempt numbering identifies executions of that generation.

| `RestartPolicy` | After success | After a retryable failure |
| --- | --- | --- |
| `Never` | Finish | Finish without another attempt. |
| `OnFailure` | Finish | Retry, subject to the retry limit. |
| `Always` | Schedule another attempt | Retry, subject to the retry limit. |

`RestartPolicy::periodic(milliseconds)` requests an `Always` interval. It is implemented by Taskvisor's attempt loop, not by a calendar scheduler. An interval or backoff wait is cancellable.

`max_retries` bounds consecutive failure retries after the initial attempt. `None` leaves that count unbounded; zero is not representable. A successful attempt resets the consecutive-failure backoff count. Fatal and cancellation results stop the task instead of taking the ordinary retry path.

Backoff defaults are full jitter, a 1,000 ms initial delay, a 30,000 ms cap, and factor 2.0. A valid backoff has a positive first delay, a cap no smaller than the first delay, and a finite factor of at least one.

The required positive `TaskSpec.timeout` is per attempt. It does not include reconciliation, controller queueing, attempt-permit waits, or retry backoff. Taskvisor starts its timer after `Task::spawn` returns the attempt future. A blocking `spawn`, poll, or destructor cannot be preempted by a Tokio timer.

Ordinary panics while constructing or polling an attempt future become retryable attempt failures. An actor or protected-cleanup panic is a different terminal outcome. Do not treat every panic diagnostic as the same lifecycle category.

## A terminal phase can precede another attempt

Core projects attempt events into status. For a retrying task, `Failed` or `Timeout` can be followed by another `Running` attempt. For an `Always` task, `Succeeded` can also be followed by another attempt.

Core separately consumes the direct final Taskvisor outcome. This finalization can correct a terminal-looking intermediate phase. `TaskPhase::is_terminal()` and `TaskFilter::with_terminal()` classify the current phase only; they do not establish that retries, interval waits, or physical execution have ended.

The final typed mapping is:

| Taskvisor result | SDK phase |
| --- | --- |
| `Completed` | `Succeeded` |
| `Failed` | `Exhausted` |
| `Fatal` or `Panicked` | `Failed` |
| `Canceled` or `ForceAborted` | `Canceled` |
| Rejection: slot busy, removed from queue, superseded replacement, controller shutdown | `Canceled` |
| Other rejection | `Failed` |

`Exhausted` includes a failure stopped by `Never`, not only a reached numeric retry limit. A configured attempt timeout can appear as `Timeout` in attempt history and later as `Exhausted` in the final resource projection when restart policy stops the runtime.

An accepted command can be rejected before attempt one. A terminal task with attempt zero is therefore valid. Rejection diagnostics do not imply that user task code entered.

## Events and authoritative completion

Taskvisor event delivery is bounded and best-effort. Core uses it for attempt history and intermediate observations, then uses a direct waiter for final runtime outcome and binding cleanup. Increasing event capacity does not convert the event stream into a durable journal.

This distinction matters for timing as well as correctness: a start marker measures an observed event boundary, not the end of the entire managed lifecycle. See [output and history](output-and-history.md).

## Examples and source

- [Embedded lifecycle example](../crates/solti-core/examples/embedded_lifecycle.rs): observe a running generation and replace it.
- [Policy contracts](../crates/solti-model/src/domain/policy/mod.rs) and [task-spec fields](../crates/solti-model/src/resource/spec.rs).
- [Taskvisor policy construction](../crates/solti-core/src/runtime/reconciler.rs), [typed phase mapping](../crates/solti-core/src/map/phase.rs), and [builder configuration boundaries](../crates/solti-core/src/supervisor/builder.rs).

## See also

- [Reconciliation](reconciliation.md) for construction and command intake.
- [Cancellation and shutdown](cancellation-and-shutdown.md) for logical and physical cleanup.
- [Configuration](configuration.md) for capacity settings.
