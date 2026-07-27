# solti-prometheus

`solti-prometheus` is the metrics backend for the Solti SDK.
Other crates (`solti-runner`, `solti-api`, `solti-discover`, `solti-core`) define metrics traits; this crate implements them against one shared Prometheus `Registry` and can expose it over `/metrics`.

## Quick start

```rust
use solti_prometheus::{PrometheusRunnerMetrics, Registry, register_build_info};
use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};

fn main() -> Result<(), prometheus::Error> {
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
| `server`                                 | Shared registry, listen address, revision  | `TaskManifest` and embedded `TaskRef`                   |

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

`Registry` and `register_build_info` are always available.
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
             │
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
use solti_prometheus::{PrometheusApiMetrics, PrometheusDiscoverMetrics, Registry};

fn main() -> Result<(), prometheus::Error> {
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
use solti_prometheus::{PrometheusCoreStateCollector, Registry};

fn main() -> Result<(), prometheus::Error> {
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

The task serves only `GET /metrics`.
It uses `AdmissionPolicy::Replace` in the `solti-metrics-server` slot.
It restarts after exit and backs off from 1 to 30 seconds after failure.
The embedded revision includes the listen address.

## Specific behavior

- `solti_taskvisor_attempts_in_flight` follows a best-effort event stream and can drift after dropped events.
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

Constructors and registration functions return `prometheus::Error`.
Registering the same metric group twice in one registry returns `AlreadyReg`.
`server` also validates the embedded revision and returns `solti_model::ModelError`.
