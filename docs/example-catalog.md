---
title: Example catalog
description: Choose among all SDK example programs by process, package, required features, and external prerequisites.
---

# Example catalog

This catalog covers all 40 Rust examples in the workspace. Start with a
cross-crate program when learning how to assemble an agent; use a component
example to examine a smaller contract.

Examples remain in their owning packages. This project-level catalog connects
them to the processes they demonstrate. It does not turn each package into a
separate documentation journey.

## Run an example

Run from the repository root:

```sh
cargo run -p solti --example task_subprocess --locked --features core,exec-subprocess
cargo run -p solti-core --example collections --locked
cargo run -p solti-model --example task_manifest_schema --locked --features schema
```

For other rows, use `cargo run -p PACKAGE --example NAME --locked` and add
`--features` with the listed comma-separated names when the row has features.
Names are package-local: a facade feature such as `exec-subprocess` is not the
direct exec package's `subprocess` feature.

The tables identify required feature gates, not every transitively enabled
feature. Package defaults still apply. See [installation](installation.md).
Read a row's prerequisites before running code that binds a port, starts a
process, or creates container resources.

## Start with a cross-crate process

These ten programs use package `solti`.

| Example | Required features | Process and prerequisites |
|---|---|---|
| [`task_subprocess`](../crates/solti/examples/task_subprocess.rs) | `core,exec-subprocess` | Router → core → real child process → output/status/history → cleanup. Runs its own binary; needs a supported subprocess platform, not a shell. |
| [`task_custom_workload`](../crates/solti/examples/task_custom_workload.rs) | `core` | Custom `TcpProbe` GVK → payload validation → core reconciliation → real local TCP connection. Binds a local test service. |
| [`task_chain`](../crates/solti/examples/task_chain.rs) | `chain,core,exec-subprocess` | Four subprocess steps with a failure/recovery path, one outer resource, and shared output. Runs its own binary. |
| [`task_containerd`](../crates/solti/examples/task_containerd.rs) | `core,exec-containerd` | Native containerd → runner → desired-state lifecycle and cleanup. Requires Linux, containerd 2.x, runtime/snapshotter plugins, and an available image. |
| [`agent_http`](../crates/solti/examples/agent_http.rs) | `api-core-adapter,api-http,exec-subprocess` | Subprocess agent with HTTP Task API and application-mounted OpenAPI at `127.0.0.1:8085`. Runs until shutdown; no authentication or TLS. |
| [`agent_http_discovery`](../crates/solti/examples/agent_http_discovery.rs) | `api-core-adapter,api-http,discover-http,exec-subprocess` | Inbound HTTP API plus outbound supervised discovery. Needs a compatible control plane at `SOLTI_CONTROL_PLANE`; default `http://127.0.0.1:8090`. |
| [`agent_grpc`](../crates/solti/examples/agent_grpc.rs) | `api-core-adapter,api-grpc,exec-subprocess` | Subprocess/core resource read through a local gRPC server and generated client with bearer metadata. |
| [`agent_grpc_mtls`](../crates/solti/examples/agent_grpc_mtls.rs) | `api-core-adapter,api-grpc-tls,exec-subprocess` | Local gRPC/core integration with mandatory client certificates and a teaching CA. Generated credentials are for the example, not deployment. |
| [`operations_observe`](../crates/solti/examples/operations_observe.rs) | `core,exec-subprocess,observe-timezone-sync` | Global logging plus supervised timezone maintenance and real subprocess work. Local-time offset detection can fail in the host environment. |
| [`operations_prometheus`](../crates/solti/examples/operations_prometheus.rs) | `core,exec-subprocess,prometheus,prometheus-server,prometheus-state` | Shared registry, runner/Taskvisor metrics, core-state collector, supervised exporter, and a real scrape. Default metrics address is `127.0.0.1:9090`; `SOLTI_METRICS_ADDR` overrides it. |

Use [Task API](serving-api.md) for current UID-bound output requests, including
the required `taskUid` parameter. Use [agent assembly](building-an-agent.md)
for shutdown ownership and long-lived stream drain.

## Understand resources and core lifecycle

| Package | Example | Required features | What it demonstrates |
|---|---|---|---|
| `solti-model` | [`task_manifest`](../crates/solti-model/examples/task_manifest.rs) | — | Caller-owned desired state, environment/policy, serialization, and strict unknown-field rejection; no execution. |
| `solti-model` | [`task_manifest_schema`](../crates/solti-model/examples/task_manifest_schema.rs) | `schema` | Generate JSON Schema and validate a real serialized manifest; schema does not replace runtime invariants. |
| `solti-model` | [`task_lifecycle`](../crates/solti-model/examples/task_lifecycle.rs) | — | Server metadata, status transitions, metadata/spec updates, and stale-generation rejection at the model layer. |
| `solti-model` | [`task_query`](../crates/solti-model/examples/task_query.rs) | — | Filters, selectors, page limits, and continuation values; constructing a query does not execute it. |
| `solti-core` | [`embedded_lifecycle`](../crates/solti-core/examples/embedded_lifecycle.rs) | — | Submit a manifest/TaskRef pair, replace its revision, cancel old work cooperatively, and inspect retained runs. |
| `solti-core` | [`collections`](../crates/solti-core/examples/collections.rs) | — | Snapshot pagination and filter-relative watch events across live mutations. |
| `solti-core` | [`routed_output`](../crates/solti-core/examples/routed_output.rs) | — | Custom runner, selector, desired-state reconciliation, gated live output, and final observed state. |

Continue with [task resources](task-resources.md), [management](managing-tasks.md),
[collections](collections-and-watches.md), and [output/history](output-and-history.md).

## Build or adapt execution

| Package | Example | Required features | What it demonstrates and needs |
|---|---|---|---|
| `solti-runner` | [`custom_extension`](../crates/solti-runner/examples/custom_extension.rs) | — | Two runners for one custom GVK, label selection, captured capabilities, and a build result without starting it. |
| `solti-runner` | [`build_context`](../crates/solti-runner/examples/build_context.rs) | — | Environment precedence, custom metrics, and attempt-scoped output sequence ownership. |
| `solti-exec` | [`subprocess_command`](../crates/solti-exec/examples/subprocess_command.rs) | `subprocess` | Real process attempt, cleared/merged environment, pinned working directory, stdout/stderr publication. |
| `solti-exec` | [`subprocess_script`](../crates/solti-exec/examples/subprocess_script.rs) | `subprocess` | Base64 decoding at build, explicit interpreter, fresh script transport, and two reusable-task attempts. Needs the example's interpreter. |
| `solti-exec` | [`host_process_policy`](../crates/solti-exec/examples/host_process_policy.rs) | `host-process` | Unix process controls, POSIX limits, attachment to a command, and attempt cleanup; platform/permission checks still apply. |
| `solti-exec` | [`custom_container_engine`](../crates/solti-exec/examples/custom_container_engine.rs) | `container` | Engine-neutral create/output/start/wait/cleanup contract. The in-memory teaching engine does not create a real container. |
| `solti-exec` | [`containerd_config`](../crates/solti-exec/examples/containerd_config.rs) | `containerd` | Print native engine configuration without daemon I/O by default. `-- --connect` requests a real connection. |
| `solti-exec` | [`container`](../crates/solti-exec/examples/container.rs) | `containerd` | Native container lifecycle, networking, OCI policy, output, and cleanup. Requires Linux and a configured containerd 2.x environment. |

`solti-chain` is covered by the facade's `task_chain` program; it has no separate
example target. Read [routing](routing-and-custom-runners.md),
[subprocesses](subprocesses.md), [containers](containers-and-isolation.md),
and [chains](chains.md) for the complete contracts.

## Exercise network and trust boundaries

| Package | Example | Required features | What it demonstrates and needs |
|---|---|---|---|
| `solti-api` | [`core_adapter`](../crates/solti-api/examples/core_adapter.rs) | `core-adapter` | Direct core visibility versus public API hiding/rejection of Embedded resources. No public server is needed. |
| `solti-api` | [`http_contract`](../crates/solti-api/examples/http_contract.rs) | `http` | Custom handler, bearer authentication, JSON/OpenAPI, and SSE; exercises the router without a listener. |
| `solti-api` | [`grpc_contract`](../crates/solti-api/examples/grpc_contract.rs) | `grpc` | Local server/client, bearer metadata, unary list, output stream, and server shutdown. |
| `solti-discover` | [`http_sync`](../crates/solti-discover/examples/http_sync.rs) | `http` | One real discovery request to a self-contained local server, independent endpoints, uptime, and metrics. Its insecure-token opt-in is for loopback teaching only. |
| `solti-discover` | [`grpc_sync`](../crates/solti-discover/examples/grpc_sync.rs) | `grpc` | Build and inspect a discovery task without connecting by default. `-- --send` needs an external compatible discovery gRPC v1 server. |
| `solti-discover` | [`retryability`](../crates/solti-discover/examples/retryability.rs) | `grpc,http` | Classification of configuration/authentication, protocol, and transient HTTP/gRPC failures. |
| `solti-tls` | [`pem_sources`](../crates/solti-tls/examples/pem_sources.rs) | — | In-memory/file PEM, validated loaded material, redacted keys, and typed load errors. Creates teaching certificate material. |
| `solti-tls` | [`tls_round_trip`](../crates/solti-tls/examples/tls_round_trip.rs) | — | Local rustls server/client, explicit roots and server name, ALPN, and encrypted exchange using a teaching PKI. |
| `solti-tls` | [`mtls_round_trip`](../crates/solti-tls/examples/mtls_round_trip.rs) | — | Mandatory client identity and independent trust roots; both peers complete an encrypted exchange using a teaching PKI. |

See [Task API](serving-api.md), [discovery](discovery.md), and
[TLS and authentication](tls-and-authentication.md). The examples do not define
an application's authorization, credential rotation, or deployment topology.

## Connect logging and metrics

| Package | Example | Required features | What it demonstrates |
|---|---|---|---|
| `solti-observe` | [`text_logging`](../crates/solti-observe/examples/text_logging.rs) | — | One global logger, UTC timestamps, filtering, targets, and structured fields without ANSI colors. |
| `solti-observe` | [`json_logging`](../crates/solti-observe/examples/json_logging.rs) | — | Serde configuration, defaults, filtering, and one JSON object per accepted tracing event. |
| `solti-observe` | [`timezone_sync`](../crates/solti-observe/examples/timezone_sync.rs) | `timezone-sync` | Construct the periodic manifest and TaskRef; this example stops before submission/execution. |
| `solti-prometheus` | [`shared_registry`](../crates/solti-prometheus/examples/shared_registry.rs) | — | Build info, registry gathering/text encoding, and duplicate registration; no HTTP endpoint. |
| `solti-prometheus` | [`adapter_metrics`](../crates/solti-prometheus/examples/adapter_metrics.rs) | `api,discover,runner` | Three producer-trait adapters using one registry and their label/duration contracts. |
| `solti-prometheus` | [`metrics_server`](../crates/solti-prometheus/examples/metrics_server.rs) | `server` | Construct exporter manifest/TaskRef and composed revision; this example stops before binding/serving. |

Use [observability](observability.md) to connect those constructors to actual
supervision, metric producers, and service lifetime. The complete operations
programs are in the first table.

## Keep the catalog complete

When adding or removing an example target, update this catalog, its package
feature gates, and the guide for the process it demonstrates. Keep a distinction
between constructing a reusable task and actually executing it, and between a
teaching adapter and a real external backend.

The [API reference map](api-reference.md) links each component's public entry
points. [Process benchmarks](../benches/README.md) have a separate purpose:
they measure explicit process boundaries rather than serve as application examples.
