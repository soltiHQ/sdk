# solti-prometheus

> Prometheus metrics for Solti agents.

`solti-prometheus` gives Solti crates one shared Prometheus registry.
Runner metrics, taskvisor events, API requests, discovery heartbeat, process stats, build info, and `/metrics` can all use that same registry.

The crate is modular. Use only the collectors your agent needs.

## The metrics wiring you stop repeating

Most agents need the same setup:

```rust,ignore
let registry = prometheus::Registry::new();
// create runner metrics
// create taskvisor subscriber
// add build info
// expose /metrics
```

With `solti-prometheus`, each part has a small collector:

```rust,no_run
use std::sync::Arc;

use solti_prometheus::{
    PrometheusMetrics, PrometheusSubscriber, Registry, register_build_info,
    register_process_collector,
};
use solti_runner::{BuildContext, RunnerRouter};
use taskvisor::Subscribe;

fn main() -> Result<(), prometheus::Error> {
    let registry = Arc::new(Registry::new());

    let runner_metrics = PrometheusMetrics::new(registry.clone())?;
    let supervisor_metrics = PrometheusSubscriber::new(registry.clone())?;

    register_process_collector(&registry)?;
    register_build_info(&registry, &[("version", env!("CARGO_PKG_VERSION"))])?;

    let ctx = BuildContext::default().with_metrics(Arc::new(runner_metrics));
    let router = RunnerRouter::new().with_context(ctx);

    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(supervisor_metrics)];

    let _ = (router, subscribers);
    Ok(())
}
```

## Quick Start

### Runner and Supervisor Metrics

These two collectors are always available:

```rust
use std::sync::Arc;

use solti_prometheus::{PrometheusMetrics, PrometheusSubscriber, Registry};
use solti_runner::{MetricOutcome, MetricsBackend, RunnerType};
use taskvisor::{Event, EventKind, Subscribe};

# fn main() -> Result<(), prometheus::Error> {
let registry = Arc::new(Registry::new());

let runner_metrics = PrometheusMetrics::new(registry.clone())?;
runner_metrics.record_task_started(RunnerType::Subprocess);
runner_metrics.record_task_completed(
    RunnerType::Subprocess,
    MetricOutcome::Success,
    42,
);

let supervisor_metrics = PrometheusSubscriber::new(registry.clone())?;
supervisor_metrics.on_event(&Event::new(EventKind::AttemptStarting).with_attempt(1));

assert!(!registry.gather().is_empty());
# Ok(()) }
```

### Build Info

Add one constant gauge with labels that identify the running binary:

```rust
use solti_prometheus::{Registry, register_build_info};

# fn main() -> Result<(), prometheus::Error> {
let registry = Registry::new();
register_build_info(&registry, &[
    ("version", env!("CARGO_PKG_VERSION")),
    ("git_sha", "unknown"),
])?;
# Ok(()) }
```

### `/metrics` Server

Enable the `server` feature to build a supervised embedded task that serves `/metrics`:

```rust,no_run
use std::sync::Arc;
use solti_prometheus::{Registry, server};

let registry = Arc::new(Registry::new());
let (task, spec) = server(registry, "0.0.0.0:9090");

// Submit to solti-core:
// supervisor.submit_with_task(task, &spec).await?;
# let _ = (task, spec);
```

## What Ships

| Component                    | Metrics                      | Feature    | Use it for                            |
|------------------------------|------------------------------|------------|---------------------------------------|
| `PrometheusMetrics`          | `solti_runner_*`             | always     | Runner execution metrics              |
| `PrometheusSubscriber`       | `solti_sv_*`, `solti_ctrl_*` | always     | Taskvisor and controller events       |
| `PrometheusApiMetrics`       | `solti_api_*`                | `api`      | HTTP/gRPC API request metrics         |
| `PrometheusDiscoverMetrics`  | `solti_discover_*`           | `discover` | Control-plane heartbeat metrics       |
| `register_process_collector` | `process_*`                  | `process`  | Process CPU, memory, file descriptors |
| `register_build_info`        | `solti_build_info`           | always     | Build identity labels                 |
| `server`                     | `/metrics` endpoint          | `server`   | Supervised HTTP metrics endpoint      |
| `PrometheusStateCollector`   | `solti_sv_tasks_by_phase`    | `state`    | Pull-based task phase snapshot        |

## Core Model

```text
Shared prometheus::Registry
  |
  |-- PrometheusMetrics         -> solti_runner_*
  |-- PrometheusSubscriber      -> solti_sv_* and solti_ctrl_*
  |-- PrometheusApiMetrics      -> solti_api_*          (feature: api)
  |-- PrometheusDiscoverMetrics -> solti_discover_*     (feature: discover)
  |-- register_process_collector -> process_*           (feature: process)
  |-- register_build_info       -> solti_build_info
  |
  v
/metrics text endpoint
```

All collectors should share one `Registry`. This gives you one scrape endpoint with all Solti metrics.

## Runner Metrics

`PrometheusMetrics` implements `solti_runner::MetricsBackend`.
Runners call it when a task starts, completes, or fails during setup.

| Metric                               | Type      | Labels              | Meaning                        |
|--------------------------------------|-----------|---------------------|--------------------------------|
| `solti_runner_tasks_started_total`   | Counter   | `runner`            | Task start events              |
| `solti_runner_tasks_completed_total` | Counter   | `runner`, `outcome` | Task completion events         |
| `solti_runner_task_duration_seconds` | Histogram | `runner`, `outcome` | Per-attempt duration           |
| `solti_runner_errors_total`          | Counter   | `runner`, `error`   | Runner setup or cleanup errors |

Duration buckets are in seconds:
`0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 300, 1800, 3600`.

## Supervisor and Controller Metrics

`PrometheusSubscriber` implements `taskvisor::Subscribe`.
It watches taskvisor events and updates supervision metrics.

| Metric                                   | Type      | Labels    | Meaning                                    |
|------------------------------------------|-----------|-----------|--------------------------------------------|
| `solti_sv_tasks_in_flight`               | Gauge     | none      | Attempts currently running, best effort    |
| `solti_sv_task_restarts_total`           | Counter   | none      | Restarts where attempt > 1                 |
| `solti_sv_task_backoff_count_total`      | Counter   | `source`  | Backoff events                             |
| `solti_sv_task_backoff_duration_seconds` | Histogram | none      | Backoff delay                              |
| `solti_sv_task_terminal_total`           | Counter   | `outcome` | Final task outcomes                        |
| `solti_sv_task_timeouts_total`           | Counter   | none      | Timeout events                             |
| `solti_sv_subscriber_overflow_total`     | Counter   | none      | Lost events in subscriber queues           |
| `solti_sv_subscriber_panicked_total`     | Counter   | none      | Subscriber panics                          |
| `solti_sv_tasks_by_phase`                | Gauge     | `phase`   | Pull-based phase snapshot, feature `state` |

Controller metrics:

| Metric                         | Type    | Labels   | Meaning                        |
|--------------------------------|---------|----------|--------------------------------|
| `solti_ctrl_submissions_total` | Counter | none     | Controller submissions         |
| `solti_ctrl_rejections_total`  | Counter | `reason` | Controller rejections by cause |

`solti_sv_tasks_in_flight` is event-based and best effort. If you need an authoritative current count, enable `state` and register `PrometheusStateCollector`.

The terminal `rejected` label is defensive. Current Taskvisor admission
rejections use `solti_ctrl_rejections_total`; they do not emit `TaskFinished`.

## API Metrics

Enable `api` to use `PrometheusApiMetrics`.

| Metric                               | Type      | Labels                                  | Meaning                    |
|--------------------------------------|-----------|-----------------------------------------|----------------------------|
| `solti_api_requests_total`           | Counter   | `transport`, `method`, `path`, `status` | Completed requests         |
| `solti_api_request_duration_seconds` | Histogram | `transport`, `method`, `path`           | Request duration           |
| `solti_api_in_flight_requests`       | Gauge     | `transport`                             | Current in-flight requests |

`path` is bounded. HTTP uses templated routes such as `/api/v1/tasks/{id}`. gRPC uses method paths from the proto service.

## Discovery Metrics

Enable `discover` to use `PrometheusDiscoverMetrics`.

| Metric                                          | Type      | Labels    | Meaning                    |
|-------------------------------------------------|-----------|-----------|----------------------------|
| `solti_discover_attempts_total`                 | Counter   | none      | Sync attempts              |
| `solti_discover_outcomes_total`                 | Counter   | `outcome` | `success` or `failure`     |
| `solti_discover_duration_seconds`               | Histogram | `outcome` | Sync call duration         |
| `solti_discover_failures_total`                 | Counter   | `reason`  | Failure reason             |
| `solti_discover_last_success_timestamp_seconds` | Gauge     | none      | UNIX time of last success  |
| `solti_discover_holds_total`                    | Counter   | none      | Server-advised retry holds |
| `solti_discover_hold_duration_seconds`          | Histogram | none      | Hold duration              |

## Process Metrics

Enable `process` to register Prometheus' standard process collector on Linux:

- `process_cpu_seconds_total`
- `process_resident_memory_bytes`
- `process_virtual_memory_bytes`
- `process_open_fds`
- `process_max_fds`
- `process_start_time_seconds`

On non-Linux targets, or without the `process` feature, `register_process_collector` is a no-op.

## Event Mapping

```text
AttemptStarting       -> tasks_in_flight.inc()
                          task_restarts.inc() if attempt > 1
AttemptSucceeded      -> tasks_in_flight.dec()
AttemptCanceled       -> tasks_in_flight.dec()
AttemptFailed         -> tasks_in_flight.dec()
AttemptTimedOut       -> tasks_in_flight.dec()
                          task_timeouts.inc()
BackoffScheduled      -> task_backoff_count{source}.inc()
                          task_backoff_duration.observe(delay)
TaskFinished          -> task_terminal{outcome}.inc()
                          tasks_in_flight.dec() for force-abort/panic fallback
SubscriberOverflow    -> subscriber_overflow.inc()
SubscriberPanicked    -> subscriber_panicked.inc()
RuntimeFailure        -> runtime_failures.inc()
ControllerSubmitted   -> controller_submissions.inc()
ControllerRejected    -> controller_rejections{reason}.inc()
```

## Label Cardinality

Prometheus labels stay low-cardinality and bounded.

| Label       | Values                                                                                 |
|-------------|----------------------------------------------------------------------------------------|
| `runner`    | `subprocess`, `wasm`, `container`                                                      |
| `outcome` (runner/discovery) | `success`, `failure`, `canceled`, `timeout`                              |
| `outcome` (task terminal) | `completed`, `exhausted`, `fatal`, `canceled`, `force_aborted`, `panicked`, `rejected`, `other`, `unknown` |
| `error`     | `cgroup_prepare_failed`, `backend_config_failed`, `spawn_failed`, `module_load_failed` |
| `source`    | `failure`, `success`                                                                   |
| `reason`    | bounded rejection and discovery reason labels                                         |
| `transport` | `http`, `grpc`                                                                         |
| `method`    | HTTP method or gRPC method name                                                        |
| `path`      | templated HTTP route or gRPC method path                                               |
| `status`    | HTTP status code or gRPC code number                                                   |

## Feature Flags

| Flag       | Default  | Effect                                      |
|------------|----------|---------------------------------------------|
| `api`      | off      | Enables `PrometheusApiMetrics`              |
| `discover` | off      | Enables `PrometheusDiscoverMetrics`         |
| `process`  | off      | Registers real `process_*` metrics on Linux |
| `server`   | off      | Enables the supervised `/metrics` HTTP task |
| `state`    | off      | Enables `PrometheusStateCollector`          |

## Notes

- All collectors should share one `prometheus::Registry`.
- `PrometheusSubscriber` uses `DEFAULT_QUEUE_CAPACITY` by default.
- Durations passed in milliseconds are converted to seconds for histograms.
- Full agent examples live in `examples/agentd-http` and `examples/agentd-grpc`.
- If a collector is registered twice in the same registry, Prometheus returns `AlreadyReg`.
