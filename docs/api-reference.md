---
title: API reference map
description: Find the public contracts, feature gates, source entry points, and process guides for every SDK product crate.
---

# API reference map

Use a process guide to understand how components cooperate, then use the owning
crate's public API for exact signatures and errors. The facade preserves those
owners through namespaces such as `solti::core` and `solti::runner`.

This is a workspace-wide map, not a generated index of only the facade. Optional
types appear only with their required features. See [installation](installation.md)
for the complete feature map.

## Locate the public contract

| Crate and source entry | Main public entry points | Process guide |
|---|---|---|
| [`solti`](../crates/solti/src/lib.rs) | Feature-gated component namespaces and Taskvisor re-export | [Architecture](architecture.md), [installation](installation.md), [agent assembly](building-an-agent.md) |
| [`solti-model`](../crates/solti-model/src/lib.rs) | `TaskManifest`, `Task`, `TaskSpec`, `TaskWorkload`, metadata/status, `TaskRun`, filters/queries/pages, `WritePreconditions`, capabilities, `Token` | [Task resources](task-resources.md), [collections](collections-and-watches.md) |
| [`solti-runner`](../crates/solti-runner/src/lib.rs) | `Runner`, `RunnerRouter`, `RunnerCatalog`, `BuiltTask`, `RunId`, `BuildContext`, build cancellation/admission, environment/output/metrics interfaces | [Routing](routing-and-custom-runners.md), [reconciliation](reconciliation.md), [output](output-and-history.md) |
| [`solti-core`](../crates/solti-core/src/lib.rs) | `SupervisorApi`, `SupervisorApiBuilder`, `TaskState`, collection/output subscriptions, configuration, persistence sinks/status, typed conflicts/errors | [Management](managing-tasks.md), [configuration](configuration.md), [persistence](persistence.md), [shutdown](cancellation-and-shutdown.md) |
| [`solti-exec`](../crates/solti-exec/src/lib.rs) | Feature-gated `subprocess`, `container`, `host`, `isolation`; runner registration, engine contracts and native adapter | [Subprocesses](subprocesses.md), [containers and isolation](containers-and-isolation.md) |
| [`solti-chain`](../crates/solti-chain/src/lib.rs) | `ChainSpec`, `ChainStep`, `FailureTransition`, `FailureMode`, `ChainRunner`, `register_chain_runner` | [Chains](chains.md) |
| [`solti-api`](../crates/solti-api/src/lib.rs) | `ApiHandler`, auth identity/requests/hooks, `ApiError`; optional `SupervisorApiAdapter`, `HttpApi`, `GrpcApi`, TLS adapter; metrics port | [Task API](serving-api.md), [TLS and authentication](tls-and-authentication.md) |
| [`solti-discover`](../crates/solti-discover/src/lib.rs) | Protocol constants, `DiscoverError`, `Retryability`, metrics port; transport-gated endpoints, `DiscoverConfig`, `sync`, uptime source | [Discovery](discovery.md) |
| [`solti-tls`](../crates/solti-tls/src/lib.rs) | `PemSource`, `PrivateKeySource`, `TlsIdentity`, `TrustRoots`, client/server configurations and loaded forms, typed errors | [TLS and authentication](tls-and-authentication.md) |
| [`solti-observe`](../crates/solti-observe/src/lib.rs) | `LoggerConfig`, format/level/timezone, `init_logger`; optional `timezone_sync` | [Observability](observability.md) |
| [`solti-prometheus`](../crates/solti-prometheus/src/lib.rs) | `Registry`, build-info registration; optional runner/API/discovery adapters, Taskvisor subscriber, core/process collectors, exporter/configuration | [Observability](observability.md) |

The standalone Taskvisor contract is an upstream dependency. Its
[public API](https://docs.rs/taskvisor/0.9.0/taskvisor/) owns `TaskRef`,
`TaskContext`, attempt outcomes, runtime/controller configuration, and subscriber
interfaces. Those are not aliases for model resource names or core TaskRun values.

## Find the detailed source guides

- [Model architecture](../crates/solti-model/ARCHITECTURE.md): resource fields, validation, desired/observed state, collection values.
- [Core architecture](../crates/solti-core/ARCHITECTURE.md): commit, reconciliation, runtime projection, state/output and cleanup.
- [Exec architecture](../crates/solti-exec/ARCHITECTURE.md): attempt ownership, host process controls, container engine and native implementation.
- [Task API contract](../crates/solti-api/CONTRACT.md): public operations, errors, transport shapes, streams, and adapter visibility.
- [Discovery contract](../crates/solti-discover/CONTRACT.md): advertised identity, heartbeat protocol, retries, and lifecycle.
- [Runner README](../crates/solti-runner/README.md) and [chain README](../crates/solti-chain/README.md): build contracts and composition.
- [TLS README](../crates/solti-tls/README.md), [observe README](../crates/solti-observe/README.md), and [Prometheus README](../crates/solti-prometheus/README.md): optional boundary-specific integrations.

These references explain individual implementations. The root guides describe
the process that uses them together. When investigating a behavior, follow the
linked public type and implementation rather than assuming every layer shares
one completion, retention, or failure boundary.

## Build local rustdoc for the selected contract

From the repository root, for example:

```sh
cargo doc -p solti-core --no-deps --locked
cargo doc -p solti --no-deps --locked --features core,exec-subprocess,api-core-adapter,api-http
```

The first command documents the core package. The second documents the facade
with one explicit agent feature set. Neither command enables every optional
SDK API. Choose feature gates that match the application you are inspecting.

For a published release, [docs.rs](https://docs.rs/solti) is the public facade
entry point. Check the selected package version before comparing it with a
local checkout. Repository source links in these guides describe the checkout.

## Maintain process coverage

When a public contract changes, update the owning crate's API documentation and
the root process guide that consumes it. A change to output delivery, for
example, can affect runner publication, core retention, API streaming,
persistence delivery, and metrics without changing their ownership boundaries.

Keep [architecture](architecture.md), [configuration](configuration.md), and
the [example catalog](example-catalog.md) aligned with those guides.
The [root overview](index.md) and [site navigation](site.yml) both list the full
guide set for repository and generated-site readers.
