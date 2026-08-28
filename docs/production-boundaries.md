---
title: Production boundaries
description: Separate the guarantees supplied by SDK components from durability, security, readiness, and lifetime policies owned by an agent application.
---

# Production boundaries

The SDK supplies validated resource contracts and bounded runtime components.
An application still owns its service contract, durable storage, network
exposure, platform permissions, and external side effects.

This guide is an integration checklist, not a claim that a particular deployment
is ready for production. Choose the rows that apply to the components you use.

## Define the acknowledgement you need

| Observation | Does not establish |
|---|---|
| Manifest passed model validation | A matching runner exists, the platform supports its controls, or a dependency is reachable. |
| Create/apply returned a Task | Runner build, slot admission, attempt success, or durable external persistence. |
| Task reached a terminal phase | Physical slot release, all destructors finished, or external side effects were undone. |
| An event callback ran | Every event was delivered or an external system committed a transaction. |
| Listener bound or capability was advertised | End-to-end readiness for a remote workload. |
| Shutdown returned at one layer | Every independently owned server, database, daemon, or cleanup worker has joined. |

Correlate observations with UID and generation. Define an outer operation
deadline when a caller needs one; the attempt timeout is not a deadline for
commit, build, admission, execution, observation, and shutdown together.
See [mental model](mental-model.md) and [agent assembly](building-an-agent.md).

## Keep reconciliation separate from transactions

Core stores desired state in memory and reconciles toward the latest generation.
Intermediate generations may be superseded before execution. A successful
external write by an old attempt is not rolled back when that generation loses
ownership. A retry can execute workload code again.

If a workload needs idempotency, external deduplication, transactional writes,
or read-back verification, its implementation and external service own those
protocols. The SDK's generation and UID guards protect SDK resource mutations;
they do not make arbitrary external effects exactly-once.

See [management](managing-tasks.md), [reconciliation](reconciliation.md), and
[custom runners](routing-and-custom-runners.md).

## Choose a durability and recovery contract

Task state, retained runs, journals, and live output have separate finite
retention rules. Continuation tokens belong to one store epoch and retained
history. They are not restart-persistent cursors.

State persistence hooks provide ordered callback delivery with pre-commit
backpressure. Output persistence hooks are best-effort. Neither installs a
database, restores state on startup, coordinates an external transaction, or
turns live output into a lossless log archive.

If historical records or restart recovery are part of your application contract,
choose and implement those outside core. Expose dispatcher health and decide
what the application does when a sink fails or stops returning.
See [persistence](persistence.md), [collections and watches](collections-and-watches.md),
and [output and history](output-and-history.md).

## Bound each retained lifetime

Core state limits, build admission, Taskvisor registry/ownership limits, output
payload limits, persistence queues, and transport limits protect different
resources. No single setting bounds the whole process's RSS.

Logical force-abort cannot interrupt arbitrary synchronous code or a blocked
destructor. Capacity can remain owned after a terminal outcome. A sink callback
that never returns can prevent its dispatcher from draining. A timed-out metrics
request can still have a collector running behind it.

Choose application-level shutdown and process-isolation policies with those
boundaries in mind. Do not treat a larger queue as a delivery guarantee.
See [configuration](configuration.md), [cancellation and shutdown](cancellation-and-shutdown.md),
and [observability](observability.md).

## Establish network access policy explicitly

The Task API's authentication and authorization hooks are opt-in. TLS/mTLS
protects connections and supplies a certificate identity when configured; it
does not select an application's authorization decisions.

The built-in authorization boundary is operation-level. It is not automatic
tenant partitioning or per-item filtering inside a returned list or stream.
Stream access is evaluated at establishment, not continuously reauthorized.
The Prometheus exporter has its own plaintext endpoint without built-in
authentication. An outbound discovery token does not secure the inbound API.

Choose listener exposure, trusted peers, credential distribution/rotation,
proxy or network controls, and handler-level isolation for the application.
See [Task API](serving-api.md), [TLS and authentication](tls-and-authentication.md),
and [discovery](discovery.md).

## Verify the backend's actual authority

A validated subprocess or container manifest does not prove that the host can
apply its controls. Host-process operations depend on platform support and
permissions. Native containerd execution depends on Linux and a suitable
containerd/runtime environment. The generic container-engine contract lets an
application supply a different implementation; its declarations are not proof
of native containerd behavior.

Names, runner labels, and workload GVKs select implementations. They are not
security boundaries. Shell and container workloads execute with the authority
provided by the agent and backend. Grant that authority according to the
workloads the application accepts.

See [subprocesses](subprocesses.md), [containers and isolation](containers-and-isolation.md),
and [routing](routing-and-custom-runners.md).

## Review one complete deployment flow

For the components you enable, record:

1. Who owns desired state and which acknowledgement clients wait for.
2. How clients recover expired watches, distinguish a recreated UID, and handle output loss.
3. Which limits protect state, construction, physical execution, and external callbacks.
4. Which external effects can be repeated and how their implementation handles repetition.
5. Which identities may call each operation and which data a handler may reveal.
6. How the advertised address, listener, backend prerequisites, and service readiness are verified.
7. Which shutdown operation joins each owner and what remains outside that boundary.

The [architecture map](architecture.md) locates each owner. The
[example catalog](example-catalog.md) identifies what each runnable example
demonstrates and the external prerequisites it leaves to the caller.
