# solti

Thin façade over the modular Solti SDK.

`solti` contains no runtime logic. It forwards features to component crates and
exposes each crate through its canonical namespace:

```rust
use solti::chain::ChainSpec;
use solti::core::SupervisorApi;
use solti::model::TaskSpec;
use solti::runner::RunnerRouter;
use solti::taskvisor::SupervisorConfig;
```

Default features are empty. Enable only the capabilities used by the binary:

```toml
[dependencies]
solti = { version = "0.0", features = ["api-core-adapter", "api-http", "chain", "core", "exec-subprocess"] }
```

The `model` feature exposes runtime model types without JSON Schema dependencies.
Enable `model-schema` when the application generates schemas for those types.
The `chain-schema` feature adds schemas for Chain and its nested model types.

The `chain` feature exposes `solti::chain`. A Chain is one Task whose nested
workloads run sequentially, with exactly one active step. The outer Task owns
restart, timeout, cancellation, status, and history; steps do not have separate
Task lifecycles or per-step policies.

`exec-container` exposes the engine-neutral container runner.
`exec-containerd` adds the native containerd 2.x engine.
It does not add CRI or container network provisioning.

`discover-http` and `discover-grpc` select an outbound transport.
Use `discover-http-tls` or `discover-grpc-tls` when that transport needs custom
TLS support.
`discover-tls` remains available as a transport-neutral extension feature.

`full` enables every production component integration. Direct dependencies on
component crates remain supported.

## Full Examples

These examples compose multiple component crates through the `solti` façade.
Every example is one compilable Rust file.
Each file starts with a flow diagram and explains its runtime result.

Names identify the example boundary:

- `task_*` calls the in-process Task lifecycle directly;
- `agent_*` assembles a binary-facing API or discovery boundary;
- `operations_*` composes metrics, logging, and maintenance integrations.

### Task Lifecycle

These examples call `SupervisorApi` directly.
They do not expose an HTTP or gRPC server.

| Example                                                         | Composition                               | Result                                                    |
|-----------------------------------------------------------------|-------------------------------------------|-----------------------------------------------------------|
| [`task_chain.rs`](examples/task_chain.rs)                       | model + runner + chain + core + exec      | Runs conditional steps and recovers a failed path         |
| [`task_subprocess.rs`](examples/task_subprocess.rs)             | model + runner + core + exec              | Runs a subprocess and observes output, state, and history |
| [`task_custom_workload.rs`](examples/task_custom_workload.rs)   | model + runner + core + Taskvisor         | Adds and executes an application-owned `TcpProbe` GVK     |
| [`task_containerd.rs`](examples/task_containerd.rs)             | model + runner + core + exec + containerd | Supervises one native containerd 2.x workload             |

Start with the local subprocess lifecycle:

```bash
cargo run -p solti --example task_subprocess \
  --features core,exec-subprocess
```

Run a conditional chain with a recovered failure:

```bash
cargo run -p solti --example task_chain \
  --features chain,core,exec-subprocess
```

Then inspect an application-defined workload:

```bash
cargo run -p solti --example task_custom_workload \
  --features core
```

Run the native containerd 2.x path on Linux:

```bash
cargo run -p solti --example task_containerd \
  --features core,exec-containerd
```

`task_containerd` requires an accessible containerd 2.x daemon.
Environment variables select the socket, namespace, snapshotter, runtime, image, and network mode.

### Agent Boundaries

| Example                                                       | Composition                              | Result                                               |
|---------------------------------------------------------------|------------------------------------------|------------------------------------------------------|
| [`agent_http.rs`](examples/agent_http.rs)                     | HTTP API + core + subprocess             | Serves the Task API and generated OpenAPI document   |
| [`agent_grpc.rs`](examples/agent_grpc.rs)                     | gRPC API + core + subprocess             | Serves the protobuf contract with bearer auth        |
| [`agent_grpc_mtls.rs`](examples/agent_grpc_mtls.rs)           | TLS + gRPC API + core + subprocess       | Rejects an anonymous peer and accepts an mTLS client |
| [`agent_http_discovery.rs`](examples/agent_http_discovery.rs) | discovery + HTTP API + core + subprocess | Advertises the capabilities of a live HTTP agent     |

Run the public API agents:

```bash
cargo run -p solti --example agent_http \
  --features api-core-adapter,api-http,exec-subprocess

cargo run -p solti --example agent_grpc \
  --features api-core-adapter,api-grpc,exec-subprocess

cargo run -p solti --example agent_grpc_mtls \
  --features api-core-adapter,api-grpc-tls,exec-subprocess
```

`agent_http` listens on `127.0.0.1:8085` until Ctrl-C.
It prints ready-to-run `curl` calls for every route.
It serves the generated OpenAPI document at `/openapi.json`.
`agent_grpc` verifies the generated client and prints `grpcurl` calls for every RPC.
It continues serving until Ctrl-C.
`agent_grpc_mtls` creates its own client and stops after one round trip.
It generates a teaching PKI in memory.

Add outbound HTTP discovery to the HTTP agent:

```bash
SOLTI_CONTROL_PLANE=http://127.0.0.1:8090 \
cargo run -p solti --example agent_http_discovery \
  --features api-core-adapter,api-http,discover-http,exec-subprocess
```

The configured control plane must implement discovery HTTP v1.
The agent continues running until Ctrl-C.

### Operations

| Example                                                           | Composition                                         | Result                                               |
|-------------------------------------------------------------------|-----------------------------------------------------|------------------------------------------------------|
| [`operations_prometheus.rs`](examples/operations_prometheus.rs)   | Prometheus + runner + core + Taskvisor + subprocess | Serves and scrapes real runtime metrics over HTTP    |
| [`operations_observe.rs`](examples/operations_observe.rs)         | observe + core + Taskvisor + subprocess             | Logs routed work and supervised timezone maintenance |

Run the operations examples:

```bash
cargo run -p solti --example operations_prometheus \
  --features core,exec-subprocess,prometheus,prometheus-server,prometheus-state

cargo run -p solti --example operations_observe \
  --features core,exec-subprocess,observe-timezone-sync
```

`operations_prometheus` serves `http://127.0.0.1:9090/metrics` until Ctrl-C.
Set `SOLTI_METRICS_ADDR` to use another listen address.
