# solti-prometheus

`solti-prometheus` is the metrics backend for the Solti SDK.
Other crates (`solti-runner`, `solti-api`, `solti-discover`, `solti-core`) define metrics traits;
this crate implements them against one shared Prometheus `Registry` and can expose it over `/metrics`.

## Quick start

```rust
use solti_prometheus::{Error, PrometheusRunnerMetrics, Registry, register_build_info};
use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};

fn main() -> Result<(), Error> {
    let registry = Registry::new();

    register_build_info(&registry, &[("version", env!("CARGO_PKG_VERSION"))])?;
    let runner = PrometheusRunnerMetrics::new(&registry)?;

    runner.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
    assert!(!registry.gather().is_empty());
    Ok(())
}
```

All integrations are disabled by default.
Enable one feature per adapter you use; the source crate then calls it during normal handling.

## What it does

- keeps all collectors in one application-owned registry;
- implements metrics traits from other Solti crates;
- converts Taskvisor events into supervision metrics;
- exposes current task phases from `TaskState`;
- registers build and process metrics;
- builds a supervised task for the `/metrics` endpoint;
- keeps adapter dependencies behind features.

## Inputs and outputs

| API                                      | Input                                      | Output                                                  |
|------------------------------------------|--------------------------------------------|---------------------------------------------------------|
| `register_build_info`                    | Registry and constant labels               | `solti_build_info` gauge                                |
| `PrometheusRunnerMetrics::new`           | Registry                                   | `solti_runner_*` collector                              |
| `PrometheusTaskvisorSubscriber::new`     | Registry                                   | Taskvisor subscriber and `solti_taskvisor_*` collectors |
| `PrometheusApiMetrics::new`              | Registry                                   | `solti_api_*` collector                                 |
| `PrometheusDiscoverMetrics::new`         | Registry                                   | `solti_discover_*` collector                            |
| `PrometheusCoreStateCollector::new`      | `TaskState`                                | Pull-based `solti_core_tasks_by_phase` collector        |
| `register_process_collector`             | Registry                                   | Standard `process_*` collectors on Linux                |
| `server` / `server_with_config`          | Shared registry, address, revision, limits | `TaskManifest` and embedded `TaskRef`                   |

Metrics adapter and subscriber constructors register their groups immediately.
`PrometheusCoreStateCollector` is returned unregistered because it implements `prometheus::core::Collector`.

## Features

| Feature                 | Public integration                         |
|-------------------------|--------------------------------------------|
| `api`                   | `PrometheusApiMetrics`                     |
| `discover`              | `PrometheusDiscoverMetrics`                |
| `process`               | `register_process_collector`               |
| `runner`                | `PrometheusRunnerMetrics`                  |
| `server`                | supervised `/metrics` task                 |
| `state`                 | `PrometheusCoreStateCollector`             |
| `taskvisor`             | `PrometheusTaskvisorSubscriber`            |
| `taskvisor-controller`  | controller metrics on the subscriber       |
| `full`                  | every integration above                    |

`Error`, `Registry`, and `register_build_info` are always available.
The default feature set does not depend on another Solti crate or Taskvisor.

## Shared registry

Use one registry for every enabled adapter:

```text
prometheus::Registry
  ├─ solti_build_info
  ├─ solti_runner_*
  ├─ solti_taskvisor_*
  ├─ solti_taskvisor_controller_*
  ├─ solti_api_*
  ├─ solti_discover_*
  ├─ solti_core_tasks_by_phase
  └─ process_*
          └─ GET /metrics
```

Each adapter registers its collectors as one group.
A name conflict rejects the complete group without leaving part of it in the registry.

## API and discovery adapters

The adapters implement the source-crate metrics traits.
The source crates call them during normal request and discovery handling.

```rust
use solti_api::{ApiMetricsBackend, Transport};
use solti_discover::{DiscoverFailReason, DiscoverMetricsBackend};
use solti_prometheus::{Error, PrometheusApiMetrics, PrometheusDiscoverMetrics, Registry};

fn main() -> Result<(), Error> {
    let registry = Registry::new();
    let api = PrometheusApiMetrics::new(&registry)?;
    let discover = PrometheusDiscoverMetrics::new(&registry)?;

    api.record_in_flight_delta(Transport::Http, 1);
    api.record_request(
        Transport::Http,
        "GET",
        "/apis/solti.io/v1/tasks/{name}",
        200,
        12,
    );
    api.record_in_flight_delta(Transport::Http, -1);

    discover.record_attempt();
    discover.record_success(25);
    discover.record_failure(50, DiscoverFailReason::Timeout);
    discover.record_hold(10);

    Ok(())
}
```

HTTP paths come from matched route templates.
gRPC paths use the full service method path.
Diagnostic text is not used as a metric label.

## Core state

The state collector counts task phases on every scrape:

```rust
use solti_core::TaskState;
use solti_prometheus::{Error, PrometheusCoreStateCollector, Registry};

fn main() -> Result<(), Error> {
    let registry = Registry::new();
    let collector = PrometheusCoreStateCollector::new(TaskState::new())?;

    registry.register(Box::new(collector))?;
    assert!(!registry.gather().is_empty());
    Ok(())
}
```

Every known phase is emitted, including phases with a zero count.
Future phase variants are aggregated under `phase="unknown"` until the crate maps them.

## Metrics server

The `server` feature builds an embedded task that serves the registry:

```rust,no_run
use std::sync::Arc;
use solti_prometheus::{Registry, server};

fn main() -> Result<(), solti_model::ModelError> {
    let registry = Arc::new(Registry::new());
    let (manifest, task_ref) = server(
        registry,
        "0.0.0.0:9090",
        "agent-registry-v1",
    )?;

    // Submit both values through the solti-core Embedded task API.
    let _ = (manifest, task_ref);
    Ok(())
}
```

The task serves only `GET /metrics`; `HEAD /metrics` returns `405` without gathering.
The endpoint is plaintext and unauthenticated. Binding `0.0.0.0` exposes it on
every available interface. A production deployment must restrict reachability
with a controlled bind address, network policy, firewall, or authenticated TLS
proxy appropriate to that deployment.
It uses `AdmissionPolicy::Replace` in the `solti-metrics-server` slot.
It restarts after exit and backs off from 1 to 30 seconds after failure.
The default scrape policy admits at most two physical gather jobs, allows a 4 MiB
encoded response, and waits up to 10 seconds for gather plus encoding before
returning `504`. The physical blocking job may outlive that response deadline and
keeps its ownership slot until it returns.
`MetricsServerConfig` can raise those values up to 16 jobs, 64 MiB, and 60 seconds.

`server_with_config` applies custom checked settings. The embedded revision includes
the listen address and every effective scrape setting.

| Outcome                                  | HTTP response                 |
|------------------------------------------|-------------------------------|
| Complete exposition                     | `200 OK`                      |
| Every scrape ownership slot is occupied | `503` with `Retry-After: 1`   |
| Response deadline elapsed                | `504 Gateway Timeout`         |
| Oversize, encoding failure, or panic     | `500 Internal Server Error`   |

Gather and encoding run on Tokio's blocking pool. A response over the byte limit is
discarded in full; the endpoint never sends a truncated Prometheus exposition.
A timed-out or disconnected request cannot cancel synchronous collector code. Its
job keeps the concurrency slot until it physically returns.
A successful job transfers its slot to the encoded `Bytes` owner. The permit follows
clones retained by the HTTP transport and returns only after the final clone drops.
Slow clients therefore cannot retain a bounded response while admitting additional
gather allocations through the same slot.

The response limit bounds the encoded body, not allocations inside a collector,
the `MetricFamily` graph returned by `Registry::gather`, or temporary encoder
values. Collector work is concurrency-bounded, not byte-bounded. Register only
trusted collectors with bounded behavior. An unwinding collector panic becomes
`500`; `panic = "abort"` cannot be isolated. Structured tracing is best effort.
A tracing subscriber panic is contained and cannot replace the scrape outcome.
A custom panic payload whose destructor also panics cannot be safely reclaimed;
its replacement payload is forgotten to contain the second unwind. Repeating that
hostile payload can leak one replacement payload per admitted panic.

Structured tracing records completed gather work plus `saturated`, `timeout`,
`response_too_large`, `encode_failed`, `scrape_panicked`, and blocking-worker
failure outcomes. The server does not register recursive self-metrics into the
application registry.

## Examples

### Internal examples

These examples stay inside the `solti-prometheus` responsibility.
Feature examples use source-crate traits because those traits form the public adapter contracts.
The server example stops at the returned manifest and `TaskRef`; it does not start a supervisor.
Each example starts with a text flow diagram, then explains its inputs, observations, and result.

Start with the application-owned registry:

```bash
cargo run -p solti-prometheus --example shared_registry
```

| Example                                           | Features              | What it shows                                                     |
|---------------------------------------------------|-----------------------|-------------------------------------------------------------------|
| [shared_registry.rs](examples/shared_registry.rs) | default               | Build information, gathering, encoding, and registration errors.  |
| [adapter_metrics.rs](examples/adapter_metrics.rs) | `api,discover,runner` | Several Solti metrics contracts sharing one registry.             |
| [metrics_server.rs](examples/metrics_server.rs)   | `server`              | Embedded manifest, `TaskRef`, revision, and the runtime boundary. |

Run the feature examples explicitly:

```bash
cargo run -p solti-prometheus --example adapter_metrics --features api,discover,runner
cargo run -p solti-prometheus --example metrics_server --features server
```

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine component crates and own the complete binary lifecycle.

## Specific behavior

- `solti_taskvisor_attempts_in_flight` correlates delivered events by Taskvisor `TaskId` and attempt.
- Duplicate terminal events and `TaskFinished` repair are idempotent for identified attempts.
- An overflow for `prometheus-taskvisor` or Taskvisor's shared `subscriber_listener`, or a conflicting active attempt identity, clears tracking and permanently sets the gauge to `NaN`.
- Overflow diagnostics for another subscriber or without a source still increment the global counter but do not invalidate this subscriber.
- Tracking remains invalid for the subscriber lifetime; later attempt events are ignored while other metrics continue to update.
- `PrometheusCoreStateCollector` recomputes phase counts from `TaskState` on each scrape.
- `register_process_collector` is a no-op on other targets.
- Build-info labels are constant and the gauge value is `1`.
- Custom runner names become `runner` label values; the application controls their cardinality.
- The default Taskvisor subscriber queue capacity is `2048` and can be overridden.
- API, discovery, and Taskvisor millisecond durations are exported in seconds.
- Discovery hold duration is already supplied in seconds.
- Taskvisor labels come from typed event fields.
- Process metrics are registered on Linux.

## Errors

Constructors and registration functions return the re-exported `solti_prometheus::Error`.
Registering the same metric group twice in one registry returns `AlreadyReg`.
`server` also validates the embedded revision and returns `solti_model::ModelError`.
