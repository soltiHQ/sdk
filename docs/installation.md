---
title: Installation and features
description: Select the SDK components, feature gates, toolchain, and platform prerequisites required by an application.
---

# Installation and features

The workspace requires Rust 1.90 or newer and uses edition 2024.
The examples below select SDK `0.0.5`, the version in the workspace
[Cargo manifest](../Cargo.toml).

## Choose the facade or a component

Use `solti` when a binary combines several components:

```toml
[dependencies]
solti = { version = "0.0.5", default-features = false, features = ["core", "exec-subprocess"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

Import components through namespaces such as `solti::core`, `solti::model`, and
`solti::exec`. Only enabled namespaces are available.
The facade has no default features and no runtime implementation.

Use a component directly when its boundary is enough:

```toml
[dependencies]
solti-model = { version = "0.0.5", default-features = false }
solti-tls = "0.0.5"
```

Direct `solti-model` and `solti-chain` dependencies enable their `schema` feature
by default. The facade disables component defaults: `solti/model` does **not**
enable JSON Schema. Select `model-schema` or `chain-schema` explicitly.

## Select a process

| Application process | Facade features | Guide |
|---|---|---|
| Construct and validate resource values | `model` | [Task resources](task-resources.md) |
| Generate model JSON Schema | `model-schema` | [Task resources](task-resources.md) |
| Define and route custom execution backends | `runner` | [Custom runners](routing-and-custom-runners.md) |
| Reconcile resources or supervise Embedded work | `core` | [Quick start](quick-start.md), [management](managing-tasks.md) |
| Execute subprocess Tasks through core | `core,exec-subprocess` | [Subprocesses](subprocesses.md) |
| Execute through an application container engine | `core,exec-container` | [Containers](containers-and-isolation.md) |
| Execute native containerd Tasks | `core,exec-containerd` | [Containers](containers-and-isolation.md) |
| Compose subprocess steps in a Chain | `chain,core,exec-subprocess` | [Chains](chains.md) |
| Serve the HTTP API with core and subprocess execution | `api-core-adapter,api-http,exec-subprocess` | [Task API](serving-api.md) |
| Serve the gRPC API with core and subprocess execution | `api-core-adapter,api-grpc,exec-subprocess` | [Task API](serving-api.md) |
| Add gRPC server TLS/mTLS | `api-grpc-tls` with the selected handler/backend features | [TLS](tls-and-authentication.md) |
| Advertise an HTTP-discovered core agent | `core,discover-http` plus its actual inbound API/backend features | [Discovery](discovery.md) |
| Configure logging and supervise timezone refresh | `core,observe-timezone-sync` | [Observability](observability.md) |
| Export core state and runner/Taskvisor metrics | `core,prometheus,prometheus-state,prometheus-server` plus the selected backend | [Observability](observability.md) |

The higher-level features enable their required lower layers.
For example, `core` already enables `model`, `runner`, and the Taskvisor controller.
`api-http` and `api-grpc` do not enable core; `api-core-adapter` does.
Discovery transport selection is independent of the agent's inbound API transport.

## Read the complete feature families

The [facade manifest](../crates/solti/Cargo.toml) is the exact forwarding contract.

| Family | Features and purpose |
|---|---|
| Data and construction | `model`, `model-schema`, `runner`, `chain`, `chain-schema`, `core`. |
| Execution | `exec` exposes the base namespace; `exec-host-process` exposes host policy; `exec-subprocess` adds the subprocess runner; `exec-container` adds the engine-neutral container runner; `exec-containerd` adds the native adapter. |
| Seccomp | `exec-seccomp` enables the low-level host-process filter. Combine it with `exec-subprocess` to apply it to subprocess attempts. |
| API | `api` exposes the handler/auth/metrics contracts; `api-core-adapter` adds core integration; `api-http`, `api-grpc`, and `api-grpc-tls` select transport support. |
| Discovery | `discover` exposes base contracts; `discover-http` and `discover-grpc` select transport; `discover-tls` adds custom TLS; `discover-http-tls` and `discover-grpc-tls` combine the corresponding transport and TLS features. |
| Logging | `observe`, `observe-journald`, `observe-log-compat`, `observe-timezone-sync`. |
| Metrics | `prometheus-base`, `prometheus-api`, `prometheus-discover`, `prometheus-process`, `prometheus-runner`, `prometheus-server`, `prometheus-state`, `prometheus-taskvisor`, `prometheus-taskvisor-controller`. |
| Metrics bundles | `prometheus` selects runner and Taskvisor-controller adapters. `prometheus-full` selects every Prometheus adapter. Neither starts an exporter. |
| TLS types | `tls` exposes the standalone TLS/mTLS configuration crate. It does not start a listener. |
| Taskvisor forwarding | `taskvisor`, `taskvisor-controller`, `taskvisor-logging`, `taskvisor-tracing`, `taskvisor-tokio-util-interop`. |
| Full integration | `full` selects the standard integration set, including native containerd, seccomp, network transports, schema, and operations. Platform/runtime prerequisites still apply. |

Features make code available. They do not install a runner, start core, expose
an endpoint, install the global logger, or submit a maintenance task.
Those are application actions described in [Build an agent](building-an-agent.md).

## Check platform prerequisites

| Path | Requirement |
|---|---|
| Embedded or custom async work | A supported Tokio runtime and the application's own dependencies. |
| Subprocess runner | Linux or macOS for the documented supported path; the requested executable or script interpreter must exist. |
| Optional host controls | Unix or Linux depending on the control. Unsupported configured controls fail closed. |
| Native containerd | Linux at execution time, containerd major version 2, configured runtime/snapshotter plugins, an accessible image, and a shared visible I/O root. |
| Native containerd build | `protoc` when compiling the `containerd-client` bindings. This is separate from connecting to a running daemon. |
| API agent programs | An available listen address; TLS examples also need the indicated identity/trust setup. The component HTTP contract example exercises the router without binding a listener. |
| Discovery in an agent | A compatible control-plane endpoint; the SDK does not provide that server. The component HTTP example starts its own teaching endpoint, and the gRPC example does not connect unless requested. |

Building an optional adapter on a host does not prove it can run there.
The non-Unix subprocess implementation is not a claim of current Windows support.
Read [subprocess platform boundaries](subprocesses.md) and
[container prerequisites](containers-and-isolation.md) before selecting host policy.

## Run from a checkout

The smallest existing lifecycle example needs no external process or server:

```sh
cargo run -p solti-core --example embedded_lifecycle --locked
```

For a routed subprocess lifecycle:

```sh
cargo run -p solti --example task_subprocess --locked --features core,exec-subprocess
```

The [example catalog](example-catalog.md) records each program's package and
required features. Use the [quick start](quick-start.md) for a standalone main
program with explicit observation and shutdown.

Source: [workspace versions](../Cargo.toml), [facade features](../crates/solti/Cargo.toml),
[model defaults](../crates/solti-model/Cargo.toml),
[chain defaults](../crates/solti-chain/Cargo.toml), and
[execution platforms](../crates/solti-exec/README.md).
