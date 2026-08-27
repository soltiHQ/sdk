---
title: Common mistakes
description: Recognize mismatched identities, acknowledgement boundaries, feature gates, retention assumptions, and ownership decisions in SDK integrations.
---

# Common mistakes

These are integration traps implied by the public contracts, not a diagnosis
of your application. Use the linked process guide to check the relevant boundary.

## Waiting for the wrong acknowledgement

| Mistake | What the contract says | Use instead |
|---|---|---|
| Treating `create_task` or apply as successful execution | The reply acknowledges desired-state commit. Build and admission can fail later. | Observe the committed UID/generation through Task state. |
| Opening a watch after a fast task finished and expecting a live completion event | A live watch does not manufacture an event that happened before its start point. | Open the watch before submission, or use a defined initial snapshot/list-watch flow. |
| Waiting by resource name alone | A deleted name can be reused by a new UID. | Match UID and the intended generation; use write guards for later mutations. |
| Treating `Succeeded`, `Failed`, or `Canceled` as an idle-slot signal | Logical status and physical ownership have different boundaries. | Follow the documented admission and cleanup contract. |
| Treating `DropIfRunning` as a queue | It can reject work behind a busy owner. | Choose `Queue` or `Replace` only when that is the desired task policy. |

Read [quick start](quick-start.md), [management](managing-tasks.md),
[lifecycle and admission](lifecycle-and-admission.md), and
[cancellation and shutdown](cancellation-and-shutdown.md).

## Confusing identities and revisions

Task name, slot, UID, generation, resource version, run ID, and attempt number
are not interchangeable. A resource version is an opaque concurrency or
observation token, not a generation counter. Task and TaskRun collection tokens
are not interchangeable either.

Applying the same desired spec does not request a new generation. It is
normally a no-op, but can retry failed reconciliation within the same generation.
Replacing the implementation behind an Embedded TaskRef without changing its
desired revision does not declare a new implementation generation.

See [task resources](task-resources.md), [management](managing-tasks.md), and
[reconciliation](reconciliation.md).

## Expecting features to start services

- The facade's default feature set is empty.
- `model` does not enable facade JSON Schema support; use `model-schema` when needed.
- `exec` alone does not select the subprocess or native containerd implementation.
- HTTP/gRPC API features do not start a server or automatically enable the core adapter.
- Discovery transport features do not choose the inbound Task API transport.
- Logging, metrics adapters, and maintenance-task features do not connect every producer or submit their tasks.

Start from [installation](installation.md), then follow [agent assembly](building-an-agent.md).

## Assuming router registration validates every future workload

Capabilities are declarations captured at registration. Routing chooses the
first matching registration by workload GVK and optional runner labels. Two
matching runners do not produce an automatic ambiguity error.

Model validation does not check runner availability or platform authority.
Custom runner build and runtime validation remain separate steps. The model's
WASM workload representation does not provide a built-in WASM executor.

See [routing and custom runners](routing-and-custom-runners.md) and
[containers and isolation](containers-and-isolation.md).

## Treating a chain as several independently managed Tasks

A chain is one outer resource and attempt. Its nested steps do not acquire
independent core Task identities, slot policies, retained run histories, or
automatic per-step retries. Conditional paths still have build-time requirements.
An outer retry can repeat earlier steps.

See [chains](chains.md) before relying on a conditional step to hide an invalid
or unavailable nested workload.

## Treating bounded observation as durable history

| Observation path | Recovery or limit to account for |
|---|---|
| Task list continuation or watch | Its journal position can expire; restart from a fresh snapshot instead of parsing the token. |
| TaskRun history | TTL, count, and byte budgets can remove values independently of Task retention. |
| Live output | Slow readers and aggregate payload pressure can produce `Lagged`; oversized chunks can be truncated. |
| Taskvisor subscriber events | Delivery is best-effort; queue size is not an authoritative lifecycle history. |
| State persistence hook | Ordered callback delivery does not implement a durable database or restart restore. |
| Output persistence hook | The external copy can be dropped independently of the live stream. |

An API output subscription needs the current Task UID as well as the name.
Do not reuse a logs URL across resource recreation without resolving that UID.
See [collections](collections-and-watches.md), [output and history](output-and-history.md),
[Task API](serving-api.md), and [persistence](persistence.md).

## Losing an owner during shutdown

Do not return early from server error handling before attempting owned cleanup.
Core shutdown, backend finalization, server connection drain, and database
shutdown are separate operations. Preserve their results independently.

Do not call and await core shutdown from a persistence callback. Direct polling
on that worker is rejected; making it wait for a different thread performing
the shutdown can still deadlock. A callback must be able to return for its
dispatcher to drain.

Stopping HTTP intake does not complete an already-open watch or output stream.
Coordinate connection drain with the stream owners and the application's outer
shutdown policy. See [agent assembly](building-an-agent.md) and
[cancellation and shutdown](cancellation-and-shutdown.md).

## Conflating transport security, authorization, and discovery

TLS does not choose who may mutate a Task. An authorizer does not automatically
filter collection items by tenant. Discovery advertises an address but does not
bind it, validate external reachability, or protect the inbound listener.
The metrics exporter is a separate endpoint with separate access controls.

See [TLS and authentication](tls-and-authentication.md), [discovery](discovery.md),
and [observability](observability.md).
